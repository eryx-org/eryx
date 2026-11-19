//! The hybrid VFS for WASI 0.3 (`wasi:filesystem@0.3.0`) guests.
//!
//! Every operation delegates to the WASI 0.2 implementation in `hybrid_host`
//! on the same [`HybridVfsState`] and resource table, and only converts
//! types. Path resolution, mount boundaries, permissions, read-only mounts,
//! the storage quota and the open-host-file limit therefore live in one
//! place and can't drift between the two WASI versions.
//!
//! The p3 interface replaces p2's `input-stream`/`output-stream` resources
//! and the directory-entry iterator with component-model streams. Those are
//! adapters over the same p2 stream objects (`streams.rs`), which carry the
//! read-size cap and the write-side quota and file-size checks.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, Resource, Source, StreamConsumer, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::{AsContextMut, StoreContextMut};
use wasmtime_wasi::p3::bindings::clocks::system_clock::Instant;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream, StreamError};

use crate::hybrid::{HybridDescriptor, HybridVfsState};
use crate::hybrid_bindings::{
    self as p2, HybridFilesystem, HybridFsError, HybridFsResult, types as p2t,
};
use crate::hybrid_host::{DirectoryListing, link_at_now, stream_error_code};
use crate::storage::VfsStorage;
use crate::streams::MAX_READ_CHUNK;

mod generated {
    pub use crate::hybrid::HybridDescriptor;
    pub use crate::hybrid_bindings::HybridFsError;

    wasmtime::component::bindgen!({
        path: "wit-p3",
        world: "virtual-filesystem",
        trappable_error_type: {
            "wasi:filesystem/types.error-code" => HybridFsError,
        },
        with: {
            "wasi:filesystem/types.descriptor": HybridDescriptor,
            "wasi:clocks/system-clock": wasmtime_wasi::p3::bindings::clocks::system_clock,
        },
        imports: {
            // These return component-model streams, so they need the store.
            "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
            // Closing the last descriptor on an unlinked file deletes it.
            "wasi:filesystem/types.[drop]descriptor": async | store | trappable,
            // WIT `async func`s always get `&Accessor`.
            default: trappable,
        },
        require_store_data_send: true,
    });
}

pub use generated::wasi::filesystem::{preopens, types};
use types::ErrorCode;

/// Largest single write a write stream hands to storage (64 KiB), matching
/// what the p2 output streams advertise through `check-write`.
const MAX_WRITE_CHUNK: usize = 64 * 1024;

// ============================================================================
// Type conversions
// ============================================================================

fn error_code(code: p2t::ErrorCode) -> ErrorCode {
    use p2t::ErrorCode as E;
    match code {
        E::Access => ErrorCode::Access,
        E::WouldBlock => ErrorCode::Other(Some("would-block".to_string())),
        E::Already => ErrorCode::Already,
        E::BadDescriptor => ErrorCode::BadDescriptor,
        E::Busy => ErrorCode::Busy,
        E::Deadlock => ErrorCode::Deadlock,
        E::Quota => ErrorCode::Quota,
        E::Exist => ErrorCode::Exist,
        E::FileTooLarge => ErrorCode::FileTooLarge,
        E::IllegalByteSequence => ErrorCode::IllegalByteSequence,
        E::InProgress => ErrorCode::InProgress,
        E::Interrupted => ErrorCode::Interrupted,
        E::Invalid => ErrorCode::Invalid,
        E::Io => ErrorCode::Io,
        E::IsDirectory => ErrorCode::IsDirectory,
        E::Loop => ErrorCode::Loop,
        E::TooManyLinks => ErrorCode::TooManyLinks,
        E::MessageSize => ErrorCode::MessageSize,
        E::NameTooLong => ErrorCode::NameTooLong,
        E::NoDevice => ErrorCode::NoDevice,
        E::NoEntry => ErrorCode::NoEntry,
        E::NoLock => ErrorCode::NoLock,
        E::InsufficientMemory => ErrorCode::InsufficientMemory,
        E::InsufficientSpace => ErrorCode::InsufficientSpace,
        E::NotDirectory => ErrorCode::NotDirectory,
        E::NotEmpty => ErrorCode::NotEmpty,
        E::NotRecoverable => ErrorCode::NotRecoverable,
        E::Unsupported => ErrorCode::Unsupported,
        E::NoTty => ErrorCode::NoTty,
        E::NoSuchDevice => ErrorCode::NoSuchDevice,
        E::Overflow => ErrorCode::Overflow,
        E::NotPermitted => ErrorCode::NotPermitted,
        E::Pipe => ErrorCode::Pipe,
        E::ReadOnly => ErrorCode::ReadOnly,
        E::InvalidSeek => ErrorCode::InvalidSeek,
        E::TextFileBusy => ErrorCode::TextFileBusy,
        E::CrossDevice => ErrorCode::CrossDevice,
    }
}

fn descriptor_type(ty: p2t::DescriptorType) -> types::DescriptorType {
    use p2t::DescriptorType as T;
    match ty {
        T::Unknown => types::DescriptorType::Other(None),
        T::BlockDevice => types::DescriptorType::BlockDevice,
        T::CharacterDevice => types::DescriptorType::CharacterDevice,
        T::Directory => types::DescriptorType::Directory,
        T::Fifo => types::DescriptorType::Fifo,
        T::SymbolicLink => types::DescriptorType::SymbolicLink,
        T::RegularFile => types::DescriptorType::RegularFile,
        T::Socket => types::DescriptorType::Socket,
    }
}

fn instant(t: p2t::Datetime) -> Instant {
    Instant {
        // A u64 count of seconds past i64::MAX is ~292 billion years out.
        seconds: i64::try_from(t.seconds).unwrap_or(i64::MAX),
        nanoseconds: t.nanoseconds,
    }
}

fn new_timestamp(ts: types::NewTimestamp) -> HybridFsResult<p2t::NewTimestamp> {
    Ok(match ts {
        types::NewTimestamp::NoChange => p2t::NewTimestamp::NoChange,
        types::NewTimestamp::Now => p2t::NewTimestamp::Now,
        types::NewTimestamp::Timestamp(t) => {
            // The p2 implementation takes times since the epoch only.
            let seconds = u64::try_from(t.seconds).map_err(|_| {
                crate::VfsError::InvalidPath("timestamp before the epoch".to_string())
            })?;
            p2t::NewTimestamp::Timestamp(p2t::Datetime {
                seconds,
                nanoseconds: t.nanoseconds,
            })
        }
    })
}

fn stat(s: p2t::DescriptorStat) -> types::DescriptorStat {
    types::DescriptorStat {
        type_: descriptor_type(s.type_),
        link_count: s.link_count,
        size: s.size,
        data_access_timestamp: s.data_access_timestamp.map(instant),
        data_modification_timestamp: s.data_modification_timestamp.map(instant),
        status_change_timestamp: s.status_change_timestamp.map(instant),
    }
}

/// Map flags between the two bindings' flag types, which share names.
macro_rules! map_flags {
    ($from:expr, $from_ty:ty => $to:ty, [$($flag:ident),* $(,)?]) => {{
        let from: $from_ty = $from;
        let mut to = <$to>::empty();
        $(if from.contains(<$from_ty>::$flag) { to |= <$to>::$flag; })*
        to
    }};
}

fn path_flags(f: types::PathFlags) -> p2t::PathFlags {
    map_flags!(f, types::PathFlags => p2t::PathFlags, [SYMLINK_FOLLOW])
}

fn open_flags(f: types::OpenFlags) -> p2t::OpenFlags {
    map_flags!(f, types::OpenFlags => p2t::OpenFlags, [CREATE, DIRECTORY, EXCLUSIVE, TRUNCATE])
}

fn descriptor_flags(f: types::DescriptorFlags) -> p2t::DescriptorFlags {
    map_flags!(
        f,
        types::DescriptorFlags => p2t::DescriptorFlags,
        [READ, WRITE, FILE_INTEGRITY_SYNC, DATA_INTEGRITY_SYNC, REQUESTED_WRITE_SYNC, MUTATE_DIRECTORY]
    )
}

fn descriptor_flags_p3(f: p2t::DescriptorFlags) -> types::DescriptorFlags {
    map_flags!(
        f,
        p2t::DescriptorFlags => types::DescriptorFlags,
        [READ, WRITE, FILE_INTEGRITY_SYNC, DATA_INTEGRITY_SYNC, REQUESTED_WRITE_SYNC, MUTATE_DIRECTORY]
    )
}

fn hash(h: p2t::MetadataHashValue) -> types::MetadataHashValue {
    types::MetadataHashValue {
        lower: h.lower,
        upper: h.upper,
    }
}

/// The error code for a failed operation, or a trap for a host bug.
fn result_code(err: HybridFsError) -> wasmtime::Result<ErrorCode> {
    Ok(error_code(err.downcast()?))
}

// ============================================================================
// Host traits: everything delegates to the p2 implementation
// ============================================================================

impl<S: VfsStorage + Clone + 'static> types::Host for HybridVfsState<'_, S> {
    fn convert_error_code(&mut self, err: HybridFsError) -> wasmtime::Result<ErrorCode> {
        result_code(err)
    }
}

impl<S: VfsStorage + Clone + 'static> preopens::Host for HybridVfsState<'_, S> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<HybridDescriptor>, String)>> {
        p2::preopens::Host::get_directories(self)
    }
}

impl<S: VfsStorage + Clone + 'static> types::HostDescriptor for HybridVfsState<'_, S> {}

// The async functions only get the store inside `Accessor::with`, which can't
// be held across an await: each one runs the p2 checks there and awaits the
// returned storage op outside it.
impl<U, S: VfsStorage + Clone + 'static> types::HostDescriptorWithStore<U> for HybridFilesystem<S> {
    async fn drop(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> wasmtime::Result<()> {
        if let Some(delete) = store.with(|mut s| s.get().close_op(fd))? {
            let _ = delete.await;
        }
        Ok(())
    }

    async fn advise(
        _store: &Accessor<U, Self>,
        _fd: Resource<HybridDescriptor>,
        _offset: types::Filesize,
        _length: types::Filesize,
        _advice: types::Advice,
    ) -> HybridFsResult<()> {
        // Advise is a hint - we can ignore it (as on p2).
        Ok(())
    }

    async fn sync_data(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<()> {
        store.with(|mut s| s.get().sync_data_now(&fd))
    }

    async fn get_flags(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorFlags> {
        let flags = store.with(|mut s| s.get().get_flags_now(&fd))?;
        Ok(descriptor_flags_p3(flags))
    }

    async fn get_type(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorType> {
        Ok(descriptor_type(
            store.with(|mut s| s.get().get_type_now(&fd))?,
        ))
    }

    async fn set_size(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        size: types::Filesize,
    ) -> HybridFsResult<()> {
        store.with(|mut s| s.get().set_size_op(&fd, size))?.await
    }

    async fn set_times(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        let (atim, mtim) = (new_timestamp(atim)?, new_timestamp(mtim)?);
        store
            .with(|mut s| s.get().set_times_op(&fd, atim, mtim))?
            .await
    }

    async fn sync(store: &Accessor<U, Self>, fd: Resource<HybridDescriptor>) -> HybridFsResult<()> {
        store.with(|mut s| s.get().sync_now(&fd))
    }

    async fn create_directory_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        store
            .with(|mut s| s.get().create_directory_at_op(&fd, path))?
            .await
    }

    async fn stat(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorStat> {
        Ok(stat(store.with(|mut s| s.get().stat_op(&fd))?.await?))
    }

    async fn stat_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::DescriptorStat> {
        let op = store.with(|mut s| s.get().stat_at_op(&fd, path_flags(flags), path))?;
        Ok(stat(op.await?))
    }

    async fn set_times_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        let (atim, mtim) = (new_timestamp(atim)?, new_timestamp(mtim)?);
        store
            .with(|mut s| {
                s.get()
                    .set_times_at_op(&fd, path_flags(flags), path, atim, mtim)
            })?
            .await
    }

    async fn link_at(
        _store: &Accessor<U, Self>,
        _fd: Resource<HybridDescriptor>,
        _old_flags: types::PathFlags,
        _old_path: String,
        _new_fd: Resource<HybridDescriptor>,
        _new_path: String,
    ) -> HybridFsResult<()> {
        link_at_now()
    }

    async fn open_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        dflags: types::DescriptorFlags,
    ) -> HybridFsResult<Resource<HybridDescriptor>> {
        let op = store.with(|mut s| {
            s.get().open_at_op(
                &fd,
                path_flags(flags),
                path,
                open_flags(oflags),
                descriptor_flags(dflags),
            )
        })?;
        let opened = op.await?;
        store.with(|mut s| s.get().push_opened(opened))
    }

    async fn readlink_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<String> {
        store.with(|mut s| s.get().readlink_at_now(&fd, &path))
    }

    async fn remove_directory_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        store
            .with(|mut s| s.get().remove_directory_at_op(&fd, path))?
            .await
    }

    async fn rename_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        old_path: String,
        new_fd: Resource<HybridDescriptor>,
        new_path: String,
    ) -> HybridFsResult<()> {
        store
            .with(|mut s| s.get().rename_at_op(&fd, old_path, &new_fd, new_path))?
            .await
    }

    async fn symlink_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        _old_path: String,
        _new_path: String,
    ) -> HybridFsResult<()> {
        store.with(|mut s| s.get().symlink_at_now(&fd))
    }

    async fn unlink_file_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        store
            .with(|mut s| s.get().unlink_file_at_op(&fd, path))?
            .await
    }

    async fn is_same_object(
        store: &Accessor<U, Self>,
        a: Resource<HybridDescriptor>,
        b: Resource<HybridDescriptor>,
    ) -> wasmtime::Result<bool> {
        store.with(|mut s| s.get().is_same_object_now(&a, &b))
    }

    async fn metadata_hash(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::MetadataHashValue> {
        Ok(hash(
            store.with(|mut s| s.get().metadata_hash_op(&fd))?.await?,
        ))
    }

    async fn metadata_hash_at(
        store: &Accessor<U, Self>,
        fd: Resource<HybridDescriptor>,
        flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::MetadataHashValue> {
        let op = store.with(|mut s| s.get().metadata_hash_at_op(&fd, path_flags(flags), path))?;
        Ok(hash(op.await?))
    }

    fn read_via_stream(
        mut store: Access<'_, U, Self>,
        fd: Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        match store.get().input_stream(&fd, offset) {
            Ok(stream) => {
                let (tx, rx) = oneshot::channel();
                let producer = ReadProducer {
                    stream: Some(stream),
                    pending: None,
                    result: Some(tx),
                };
                Ok((
                    StreamReader::new(&mut store, producer)?,
                    FutureReader::new(&mut store, rx)?,
                ))
            }
            Err(err) => Ok((
                StreamReader::new(&mut store, std::iter::empty())?,
                ready_future(&mut store, Err(result_code(err)?))?,
            )),
        }
    }

    fn write_via_stream(
        store: Access<'_, U, Self>,
        fd: Resource<HybridDescriptor>,
        data: StreamReader<u8>,
        offset: types::Filesize,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write(store, fd, data, Some(offset))
    }

    fn append_via_stream(
        store: Access<'_, U, Self>,
        fd: Resource<HybridDescriptor>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write(store, fd, data, None)
    }

    fn read_directory(
        mut store: Access<'_, U, Self>,
        fd: Resource<HybridDescriptor>,
    ) -> wasmtime::Result<(
        StreamReader<types::DirectoryEntry>,
        FutureReader<Result<(), ErrorCode>>,
    )> {
        match store.get().directory_entries(&fd) {
            Ok(listing) => {
                let (tx, rx) = oneshot::channel();
                let producer = ReaddirProducer {
                    listing: Some(listing),
                    entries: Vec::new().into_iter(),
                    result: Some(tx),
                };
                Ok((
                    StreamReader::new(&mut store, producer)?,
                    FutureReader::new(&mut store, rx)?,
                ))
            }
            Err(err) => Ok((
                StreamReader::new(&mut store, std::iter::empty())?,
                ready_future(&mut store, Err(result_code(err)?))?,
            )),
        }
    }
}

// ============================================================================
// Streams
// ============================================================================

/// Where a stream operation reports its final result.
type ResultTx = oneshot::Sender<Result<(), ErrorCode>>;

/// A future that resolves straight away to `res`.
fn ready_future<T>(
    store: impl AsContextMut<Data = T>,
    res: Result<(), ErrorCode>,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>>
where
    T: 'static,
{
    FutureReader::new(store, async move { wasmtime::error::Ok(res) })
}

/// The error code for a failed stream operation, or a trap for a host bug.
fn stream_failure(err: StreamError) -> wasmtime::Result<Result<(), ErrorCode>> {
    match err {
        StreamError::Closed => Ok(Ok(())),
        StreamError::LastOperationFailed(e) => Ok(Err(error_code(
            stream_error_code(&e).unwrap_or(p2t::ErrorCode::Io),
        ))),
        StreamError::Trap(e) => Err(e),
    }
}

type Pending<S, R> = Pin<Box<dyn Future<Output = (S, R)> + Send>>;

/// Feeds a p2 input stream (`streams.rs`) into a p3 `stream<u8>`.
struct ReadProducer {
    stream: Option<DynInputStream>,
    pending: Option<Pending<DynInputStream, Result<Bytes, StreamError>>>,
    result: Option<ResultTx>,
}

impl ReadProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for ReadProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for ReadProducer {
    type Item = u8;
    type Buffer = Bytes;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        // Storage reads finish promptly, so a pending one is always awaited.
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        if me.pending.is_none() {
            let size = match dst.remaining(&mut store) {
                Some(0) => return Poll::Ready(Ok(StreamResult::Completed)),
                Some(n) => n.min(MAX_READ_CHUNK),
                None => MAX_READ_CHUNK,
            };
            let Some(mut stream) = me.stream.take() else {
                return Poll::Ready(Ok(StreamResult::Dropped));
            };
            me.pending = Some(Box::pin(async move {
                let res = stream.blocking_read(size).await;
                (stream, res)
            }));
        }
        let Some(pending) = me.pending.as_mut() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        let (stream, res) = ready!(pending.as_mut().poll(cx));
        me.pending = None;
        match res {
            Ok(bytes) if !bytes.is_empty() => {
                me.stream = Some(stream);
                dst.set_buffer(bytes);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Ok(_) => {
                me.close(Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Err(err) => {
                me.close(stream_failure(err)?);
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

/// Drains a p3 `stream<u8>` into a p2 output stream (`streams.rs`).
struct WriteConsumer {
    stream: Option<DynOutputStream>,
    pending: Option<Pending<DynOutputStream, Result<usize, StreamError>>>,
    result: Option<ResultTx>,
}

impl WriteConsumer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for WriteConsumer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamConsumer<D> for WriteConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, D>,
        src: Source<'_, Self::Item>,
        // Storage writes finish promptly, so a pending one is always awaited.
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let me = &mut *self;
        if me.pending.is_none() {
            let chunk = src.remaining();
            if chunk.is_empty() {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            let bytes = Bytes::copy_from_slice(&chunk[..chunk.len().min(MAX_WRITE_CHUNK)]);
            let Some(mut stream) = me.stream.take() else {
                return Poll::Ready(Ok(StreamResult::Dropped));
            };
            me.pending = Some(Box::pin(async move {
                let n = bytes.len();
                let res = stream.blocking_write_and_flush(bytes).await.map(|()| n);
                (stream, res)
            }));
        }
        let Some(pending) = me.pending.as_mut() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        let (stream, res) = ready!(pending.as_mut().poll(cx));
        me.pending = None;
        match res {
            Ok(n) => {
                me.stream = Some(stream);
                src.mark_read(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(err) => {
                me.close(stream_failure(err)?);
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

/// Lists a directory once, then streams its entries.
struct ReaddirProducer {
    listing: Option<DirectoryListing>,
    entries: std::vec::IntoIter<p2t::DirectoryEntry>,
    result: Option<ResultTx>,
}

impl ReaddirProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for ReaddirProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for ReaddirProducer {
    type Item = types::DirectoryEntry;
    type Buffer = Option<types::DirectoryEntry>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        if let Some(listing) = me.listing.as_mut() {
            let res = ready!(listing.as_mut().poll(cx));
            me.listing = None;
            match res {
                Ok(entries) => me.entries = entries.into_iter(),
                Err(err) => {
                    me.close(Err(result_code(err)?));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
        match me.entries.next() {
            Some(p2t::DirectoryEntry { type_, name }) => {
                dst.set_buffer(Some(types::DirectoryEntry {
                    type_: descriptor_type(type_),
                    name,
                }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => {
                me.close(Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

fn write<T, S: VfsStorage + Clone + 'static>(
    mut store: Access<'_, T, HybridFilesystem<S>>,
    fd: Resource<HybridDescriptor>,
    mut data: StreamReader<u8>,
    offset: Option<types::Filesize>,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    match store.get().output_stream(&fd, offset) {
        Ok(stream) => {
            let (tx, rx) = oneshot::channel();
            let consumer = WriteConsumer {
                stream: Some(stream),
                pending: None,
                result: Some(tx),
            };
            data.pipe(&mut store, consumer)?;
            FutureReader::new(&mut store, rx)
        }
        Err(err) => {
            data.close(&mut store)?;
            ready_future(&mut store, Err(result_code(err)?))
        }
    }
}
