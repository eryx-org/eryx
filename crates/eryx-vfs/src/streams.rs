//! Stream implementations for hybrid VFS.
//!
//! This module provides InputStream and OutputStream implementations for
//! reading and writing files through the hybrid VFS.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

use crate::error::{VfsError, VfsResult};
use crate::file_io::{read_at, write_at};
use crate::storage::VfsStorage;

/// Largest buffer a single stream read will allocate, regardless of the length
/// the guest asked for (1 MiB).
///
/// WASI streams are allowed to return short reads, so a guest that wants more
/// simply reads again.
const MAX_READ_CHUNK: usize = 1024 * 1024;

/// An input stream for reading from a real filesystem file.
pub struct RealFileInputStream {
    /// The underlying file.
    file: Arc<std::fs::File>,
    /// Current read position.
    position: u64,
    /// Whether the stream has been closed.
    closed: bool,
}

impl RealFileInputStream {
    /// Create a new file input stream starting at the given offset.
    pub fn new(file: Arc<std::fs::File>, offset: u64) -> Self {
        Self {
            file,
            position: offset,
            closed: false,
        }
    }
}

#[async_trait::async_trait]
impl Pollable for RealFileInputStream {
    async fn ready(&mut self) {
        // File I/O is always "ready" - we do blocking reads
        // In a production system, this would use async file I/O
    }
}

#[async_trait::async_trait]
impl InputStream for RealFileInputStream {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if self.closed {
            return Err(StreamError::Closed);
        }

        if size == 0 {
            return Ok(Bytes::new());
        }

        // `size` is the guest's requested read length. Streams may return
        // short reads, so cap the buffer instead of allocating whatever was
        // asked for - otherwise a single read call sizes a host allocation.
        let mut buf = vec![0u8; size.min(MAX_READ_CHUNK)];
        match read_at(&self.file, &mut buf, self.position) {
            Ok(0) => {
                // EOF
                self.closed = true;
                Err(StreamError::Closed)
            }
            Ok(n) => {
                buf.truncate(n);
                self.position += n as u64;
                Ok(Bytes::from(buf))
            }
            Err(e) => Err(StreamError::LastOperationFailed(e.into())),
        }
    }

    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        // For file I/O, blocking_read is the same as read since file reads
        // don't actually block in the async sense (they complete synchronously)
        self.read(size)
    }
}

/// An output stream for writing to a real filesystem file.
pub struct RealFileOutputStream {
    /// The underlying file.
    file: Arc<std::fs::File>,
    /// Current write position.
    position: u64,
    /// Whether to append to the file.
    append: bool,
    /// Whether the stream has been closed.
    closed: bool,
}

impl RealFileOutputStream {
    /// Create a new file output stream for writing at a specific offset.
    pub fn write_at(file: Arc<std::fs::File>, offset: u64) -> Self {
        Self {
            file,
            position: offset,
            append: false,
            closed: false,
        }
    }

    /// Create a new file output stream for appending.
    pub fn append(file: Arc<std::fs::File>) -> Self {
        Self {
            file,
            position: 0, // Position doesn't matter for append
            append: true,
            closed: false,
        }
    }
}

#[async_trait::async_trait]
impl Pollable for RealFileOutputStream {
    async fn ready(&mut self) {
        // File I/O is always "ready"
    }
}

#[async_trait::async_trait]
impl OutputStream for RealFileOutputStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }

        if bytes.is_empty() {
            return Ok(());
        }

        let result = if self.append {
            // For append mode, get the file length and write there
            // since there's no direct append_at method
            match self.file.metadata() {
                Ok(meta) => {
                    let len = meta.len();
                    write_at(&self.file, &bytes, len)
                }
                Err(e) => Err(e),
            }
        } else {
            write_at(&self.file, &bytes, self.position)
        };

        match result {
            Ok(n) => {
                if !self.append {
                    self.position += n as u64;
                }
                Ok(())
            }
            Err(e) => Err(StreamError::LastOperationFailed(e.into())),
        }
    }

    fn flush(&mut self) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        // The sync will happen on close.
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        // We can always write - return a reasonable buffer size
        Ok(64 * 1024) // 64KB
    }

    async fn blocking_write_and_flush(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.write(bytes)?;
        self.flush()
    }
}

// ============================================================================
// VFS Stream Implementations
// ============================================================================

/// Run a storage operation if it finishes within one poll, else cancel it.
///
/// VFS storage is async while the non-blocking stream methods are sync. An
/// operation that can't finish straight away (e.g. the storage lock is
/// contended) is dropped rather than parked: a parked future would keep its
/// place in the lock queue while nobody polls it, stalling every other
/// storage op behind it. [`Pollable::ready`] reruns it to completion instead,
/// so storage operations must be safe to cancel before they finish.
fn try_now<T>(fut: impl Future<Output = T>) -> Option<T> {
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => Some(result),
        Poll::Pending => None,
    }
}

/// Wrap a storage error so `filesystem-error-code` can map it back to an
/// error code (e.g. `ENOSPC` for a full filesystem).
fn stream_error(err: VfsError) -> StreamError {
    StreamError::LastOperationFailed(wasmtime::Error::new(err))
}

/// An input stream for reading from a VFS file.
pub struct VfsInputStream<S: VfsStorage + Clone + 'static> {
    /// The VFS storage backend.
    storage: S,
    /// Path to the file.
    path: String,
    /// Current read position.
    position: u64,
    /// Size of a read that couldn't finish straight away; [`Pollable::ready`]
    /// performs it.
    wanted: Option<usize>,
    /// Result of a read performed by [`Pollable::ready`], returned by the
    /// next read.
    fetched: Option<VfsResult<Vec<u8>>>,
    /// Whether the stream has been closed.
    closed: bool,
}

impl<S: VfsStorage + Clone + 'static> VfsInputStream<S> {
    /// Create a new VFS input stream starting at the given offset.
    pub fn new(storage: S, path: String, offset: u64) -> Self {
        Self {
            storage,
            path,
            position: offset,
            wanted: None,
            fetched: None,
            closed: false,
        }
    }

    fn start_read(&self, size: usize) -> impl Future<Output = VfsResult<Vec<u8>>> + use<S> {
        let storage = self.storage.clone();
        let path = self.path.clone();
        let position = self.position;
        let size = size.min(MAX_READ_CHUNK) as u64;
        async move { storage.read_at(&path, position, size).await }
    }

    /// Turn the outcome of a finished read into at most `size` bytes of
    /// stream result.
    fn finish_read(&mut self, result: VfsResult<Vec<u8>>, size: usize) -> StreamResult<Bytes> {
        let mut data = result.map_err(stream_error)?;
        if data.is_empty() {
            self.closed = true;
            return Err(StreamError::Closed);
        }
        // A read fetched by `ready` may be longer than this call asked for;
        // the rest is read again later.
        data.truncate(size);
        self.position += data.len() as u64;
        Ok(Bytes::from(data))
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + Clone + 'static> Pollable for VfsInputStream<S> {
    async fn ready(&mut self) {
        // Readiness is the outstanding read (if any) finishing; its result is
        // picked up by the next `read`.
        if self.fetched.is_none()
            && let Some(size) = self.wanted
        {
            let result = self.start_read(size).await;
            self.wanted = None;
            self.fetched = Some(result);
        }
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + Clone + 'static> InputStream for VfsInputStream<S> {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        if size == 0 {
            return Ok(Bytes::new());
        }
        if let Some(result) = self.fetched.take() {
            return self.finish_read(result, size);
        }
        match try_now(self.start_read(size)) {
            Some(result) => {
                self.wanted = None;
                self.finish_read(result, size)
            }
            None => {
                // Nothing available yet: the guest waits on `ready` and retries.
                self.wanted = Some(size);
                Ok(Bytes::new())
            }
        }
    }

    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        if size == 0 {
            return Ok(Bytes::new());
        }
        if let Some(result) = self.fetched.take() {
            return self.finish_read(result, size);
        }
        let result = self.start_read(size).await;
        self.wanted = None;
        self.finish_read(result, size)
    }
}

/// An output stream for writing to a VFS file.
///
/// Every write goes straight to storage, so a failure (such as the
/// filesystem being full) is reported to the guest by the write or flush that
/// caused it rather than being lost.
pub struct VfsOutputStream<S: VfsStorage + Clone + 'static> {
    /// The VFS storage backend.
    storage: S,
    /// Path to the file.
    path: String,
    /// Current write position in the file, or None for append mode.
    position: Option<u64>,
    /// Bytes accepted by [`OutputStream::write`] whose write couldn't finish
    /// straight away; retried by the next stream call or [`Pollable::ready`].
    queued: Option<Bytes>,
    /// Failure of a write that finished after `write` returned, reported by
    /// the next stream call.
    error: Option<VfsError>,
    /// Whether the stream has been closed.
    closed: bool,
}

impl<S: VfsStorage + Clone + 'static> VfsOutputStream<S> {
    /// Create a new VFS output stream for writing at a specific offset.
    pub fn write_at(storage: S, path: String, offset: u64) -> Self {
        Self::new(storage, path, Some(offset))
    }

    /// Create a new VFS output stream for appending to a file.
    pub fn append(storage: S, path: String) -> Self {
        Self::new(storage, path, None)
    }

    fn new(storage: S, path: String, position: Option<u64>) -> Self {
        Self {
            storage,
            path,
            position,
            queued: None,
            error: None,
            closed: false,
        }
    }

    /// Write `bytes`, resolving to the position just past them.
    fn start_write(&self, bytes: Bytes) -> impl Future<Output = VfsResult<u64>> + use<S> {
        let storage = self.storage.clone();
        let path = self.path.clone();
        let position = self.position;
        async move {
            let position = match position {
                Some(pos) => pos,
                // Append mode: write at the current end of the file, or at 0
                // if it doesn't exist yet.
                None => storage.stat(&path).await.map_or(0, |meta| meta.size),
            };
            storage.write_at(&path, position, &bytes).await?;
            Ok(position + bytes.len() as u64)
        }
    }

    /// Record the outcome of a finished write.
    fn finish_write(&mut self, result: VfsResult<u64>) -> StreamResult<()> {
        // Appends become positional after the first write.
        self.position = Some(result.map_err(stream_error)?);
        Ok(())
    }

    /// Report the result of earlier writes; `Ok(false)` if one still can't
    /// finish.
    fn check_previous(&mut self) -> StreamResult<bool> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        if let Some(err) = self.error.take() {
            return Err(stream_error(err));
        }
        if let Some(bytes) = self.queued.clone() {
            let Some(result) = try_now(self.start_write(bytes)) else {
                return Ok(false);
            };
            self.queued = None;
            self.finish_write(result)?;
        }
        Ok(true)
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + Clone + 'static> Pollable for VfsOutputStream<S> {
    async fn ready(&mut self) {
        // `queued` is only cleared once the write is done, so dropping this
        // future part-way leaves it to be retried.
        if let Some(bytes) = self.queued.clone() {
            let result = self.start_write(bytes).await;
            self.queued = None;
            match result {
                Ok(position) => self.position = Some(position),
                Err(err) => self.error = Some(err),
            }
        }
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + Clone + 'static> OutputStream for VfsOutputStream<S> {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if !self.check_previous()? {
            return Err(StreamError::trap(
                "write not permitted: previous write still in progress",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        match try_now(self.start_write(bytes.clone())) {
            Some(result) => self.finish_write(result),
            None => {
                self.queued = Some(bytes);
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> StreamResult<()> {
        // Nothing is buffered; a still-queued write completes via `ready`.
        self.check_previous().map(|_| ())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.check_previous()? {
            Ok(64 * 1024) // 64KB
        } else {
            Ok(0)
        }
    }

    async fn blocking_write_and_flush(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.ready().await;
        self.check_previous()?;
        if bytes.is_empty() {
            return Ok(());
        }
        let result = self.start_write(bytes).await;
        self.finish_write(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::storage::{ArcStorage, InMemoryStorage};

    #[test]
    fn test_real_streams_independent_offsets_and_append() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        write_at(&file, b"hello world", 0).unwrap();
        let mut first = RealFileInputStream::new(Arc::clone(&file), 0);
        let mut second = RealFileInputStream::new(Arc::clone(&file), 6);
        let mut output = RealFileOutputStream::write_at(Arc::clone(&file), 6);
        let mut append = RealFileOutputStream::append(Arc::clone(&file));

        assert_eq!(&*first.read(5).unwrap(), b"hello");
        output.write(Bytes::from_static(b"Rust")).unwrap();
        append.write(Bytes::from_static(b"!")).unwrap();
        output.write(Bytes::from_static(b"?")).unwrap();
        assert_eq!(&*second.read(6).unwrap(), b"Rust?!");
        assert_eq!(&*first.read(7).unwrap(), b" Rust?!");
        assert!(matches!(second.read(1), Err(StreamError::Closed)));
    }

    #[tokio::test]
    async fn test_vfs_input_stream_read() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"hello world").await.unwrap();

        let mut stream = VfsInputStream::new(storage.clone(), "/test.txt".to_string(), 0);

        // Read all content
        let result = stream.read(100).unwrap();
        assert_eq!(&*result, b"hello world");

        // Reading again should return Closed (EOF)
        let err = stream.read(100).unwrap_err();
        assert!(matches!(err, StreamError::Closed));
    }

    #[tokio::test]
    async fn test_vfs_input_stream_read_at_offset() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"hello world").await.unwrap();

        // Start reading from offset 6
        let mut stream = VfsInputStream::new(storage.clone(), "/test.txt".to_string(), 6);

        let result = stream.read(100).unwrap();
        assert_eq!(&*result, b"world");
    }

    #[tokio::test]
    async fn test_vfs_input_stream_partial_read() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"hello world").await.unwrap();

        let mut stream = VfsInputStream::new(storage.clone(), "/test.txt".to_string(), 0);

        // Read only 5 bytes
        let result = stream.read(5).unwrap();
        assert_eq!(&*result, b"hello");

        // Read remaining
        let result = stream.read(100).unwrap();
        assert_eq!(&*result, b" world");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_write() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        // Create empty file first
        storage.write("/test.txt", b"").await.unwrap();

        let mut stream = VfsOutputStream::write_at(storage.clone(), "/test.txt".to_string(), 0);

        stream.write(Bytes::from("hello")).unwrap();
        stream.flush().unwrap();

        // Verify content
        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(&content, b"hello");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_write_at_offset() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"XXXXX world").await.unwrap();

        // Write at offset 0
        let mut stream = VfsOutputStream::write_at(storage.clone(), "/test.txt".to_string(), 0);

        stream.write(Bytes::from("hello")).unwrap();
        stream.flush().unwrap();

        // Verify content - should overwrite first 5 chars
        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(&content, b"hello world");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_append() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"hello").await.unwrap();

        // Append to file
        let mut stream = VfsOutputStream::append(storage.clone(), "/test.txt".to_string());

        stream.write(Bytes::from(" world")).unwrap();
        stream.flush().unwrap();

        // Verify content
        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(&content, b"hello world");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_append_to_nonexistent() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));

        // Append to non-existent file (should start at offset 0)
        let mut stream = VfsOutputStream::append(storage.clone(), "/new.txt".to_string());

        stream.write(Bytes::from("new content")).unwrap();
        stream.flush().unwrap();

        // Verify content
        let content = storage.read("/new.txt").await.unwrap();
        assert_eq!(&content, b"new content");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_multiple_appends() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"").await.unwrap();

        let mut stream = VfsOutputStream::append(storage.clone(), "/test.txt".to_string());

        stream.write(Bytes::from("one")).unwrap();
        stream.flush().unwrap();
        stream.write(Bytes::from("two")).unwrap();
        stream.flush().unwrap();
        stream.write(Bytes::from("three")).unwrap();
        stream.flush().unwrap();

        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(&content, b"onetwothree");
    }

    #[tokio::test]
    async fn test_vfs_output_stream_write_lands_without_flush() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"").await.unwrap();

        {
            let mut stream = VfsOutputStream::write_at(storage.clone(), "/test.txt".to_string(), 0);
            stream.write(Bytes::from("hello")).unwrap();
        }

        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(&content, b"hello");
    }

    /// A full filesystem must fail the write that overflows it, with an error
    /// `filesystem-error-code` can map to `ENOSPC`, instead of being dropped.
    #[tokio::test]
    async fn test_vfs_output_stream_reports_quota_error() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::with_max_bytes(4)));
        storage.write("/test.txt", b"").await.unwrap();
        let mut stream = VfsOutputStream::write_at(storage.clone(), "/test.txt".to_string(), 0);

        let err = stream
            .blocking_write_and_flush(Bytes::from("too long"))
            .await
            .unwrap_err();
        let StreamError::LastOperationFailed(err) = err else {
            panic!("expected LastOperationFailed, got {err:?}");
        };
        assert!(matches!(
            err.downcast_ref::<VfsError>(),
            Some(VfsError::QuotaExceeded(_))
        ));
        assert!(matches!(
            stream.write(Bytes::from("12345")),
            Err(StreamError::LastOperationFailed(_))
        ));
    }

    #[tokio::test]
    async fn test_vfs_streams_closed_error() {
        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        storage.write("/test.txt", b"hello").await.unwrap();

        // Read until closed
        let mut input = VfsInputStream::new(storage.clone(), "/test.txt".to_string(), 0);
        let _ = input.read(100).unwrap(); // Read all
        let _ = input.read(100); // Triggers closed

        // Subsequent reads should return Closed
        let err = input.read(100).unwrap_err();
        assert!(matches!(err, StreamError::Closed));
    }

    /// A stream write that can't take the storage lock straight away finishes
    /// later; it must not be lost or reported as done early.
    #[tokio::test]
    async fn stream_write_completes_after_contention() {
        let inner = Arc::new(InMemoryStorage::new());
        inner.write("/f", b"").await.unwrap();
        let storage = ArcStorage::new(inner.clone());
        let mut stream = VfsOutputStream::write_at(storage, "/f".to_string(), 0);

        let guard = inner.hold_lock().await;
        stream.write(Bytes::from_static(b"hello")).unwrap();
        assert_eq!(stream.check_write().unwrap(), 0, "write still in progress");
        drop(guard);

        stream.ready().await;
        assert!(stream.check_write().unwrap() > 0);
        assert_eq!(inner.read("/f").await.unwrap(), b"hello");
    }

    /// Give a storage op that should not be blocked a bounded time to finish.
    async fn finishes<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .unwrap_or_else(|_| panic!("storage op stalled behind an unpolled stream operation"))
    }

    /// A write that hit a contended lock must not keep a place in the lock
    /// queue while nobody polls it, or the next storage op stalls the store.
    #[tokio::test]
    async fn unpolled_stream_write_does_not_stall_storage() {
        let inner = Arc::new(InMemoryStorage::new());
        inner.write("/f", b"").await.unwrap();
        let mut stream = VfsOutputStream::write_at(ArcStorage::new(inner.clone()), "/f".into(), 0);

        let guard = inner.hold_lock().await;
        stream.write(Bytes::from_static(b"hello")).unwrap();
        drop(guard);

        finishes(inner.write("/g", b"other")).await.unwrap();
        finishes(stream.ready()).await;
        stream.flush().unwrap();
        assert_eq!(inner.read("/f").await.unwrap(), b"hello");
    }

    /// Same for a read: a parked read must not block a later writer.
    #[tokio::test]
    async fn unpolled_stream_read_does_not_stall_storage() {
        let inner = Arc::new(InMemoryStorage::new());
        inner.write("/f", b"hello").await.unwrap();
        let mut stream = VfsInputStream::new(ArcStorage::new(inner.clone()), "/f".into(), 0);

        let guard = inner.hold_write_lock().await;
        assert!(stream.read(5).unwrap().is_empty(), "read can't finish yet");
        drop(guard);

        finishes(inner.write("/g", b"other")).await.unwrap();
        finishes(stream.ready()).await;
        assert_eq!(&*stream.read(5).unwrap(), b"hello");
    }
}
