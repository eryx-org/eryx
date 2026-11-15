//! Host trait implementations for hybrid VFS.
//!
//! This module implements the WASI filesystem Host traits for HybridVfsState,
//! routing operations to either VFS storage or real filesystem based on
//! the descriptor type.

use std::sync::Arc;

use cap_primitives::fs as capfs;
use wasmtime::component::Resource;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

use crate::file_io::{read_at, reopen_for_append, write_at};
use crate::hybrid::{HybridDescriptor, HybridPreopen, HybridVfsState, RealDir, RealFile};
use crate::hybrid_bindings::HybridReaddirIterator;
use crate::hybrid_bindings::{DirPerms, FilePerms, HybridFsError, HybridFsResult, preopens, types};
use crate::storage::VfsStorage;
use crate::streams::{
    MAX_READ_CHUNK, RealFileInputStream, RealFileOutputStream, VfsInputStream, VfsOutputStream,
};
use crate::wasi_impl::VfsDescriptor;

// ============================================================================
// preopens::Host Implementation
// ============================================================================

impl<S: VfsStorage + Clone + 'static> preopens::Host for HybridVfsState<'_, S> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<HybridDescriptor>, String)>> {
        tracing::debug!(
            "hybrid VFS get_directories called, {} preopens configured",
            self.ctx.preopens.len()
        );
        let mut result = Vec::new();
        for preopen in &self.ctx.preopens {
            let (descriptor, path) = match preopen {
                HybridPreopen::Vfs {
                    guest_path,
                    dir_perms,
                    file_perms,
                    storage,
                } => {
                    let mut vfs_desc =
                        VfsDescriptor::dir(guest_path.clone(), *dir_perms, *file_perms);
                    vfs_desc.storage = *storage;
                    (HybridDescriptor::Vfs(vfs_desc), guest_path.clone())
                }
                HybridPreopen::Real { guest_path, dir } => {
                    let hybrid_desc = HybridDescriptor::RealDir {
                        dir: dir.clone(),
                        guest_path: guest_path.clone(),
                    };
                    (hybrid_desc, guest_path.clone())
                }
            };
            let resource = self.table.push(descriptor)?;
            tracing::debug!(
                "hybrid VFS preopen: {:?} at path {:?}",
                resource.rep(),
                path
            );
            result.push((resource, path));
        }
        tracing::debug!("hybrid VFS returning {} preopens", result.len());
        Ok(result)
    }
}

// ============================================================================
// types::Host Implementation
// ============================================================================

impl<S: VfsStorage + Clone + 'static> types::Host for HybridVfsState<'_, S> {
    fn convert_error_code(&mut self, err: HybridFsError) -> wasmtime::Result<types::ErrorCode> {
        err.downcast()
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<types::ErrorCode>> {
        Ok(stream_error_code(self.table.get(&err)?))
    }
}

/// The error code behind a failed stream operation, if it has one.
pub(crate) fn stream_error_code(err: &wasmtime::Error) -> Option<types::ErrorCode> {
    if let Some(vfs_err) = err.downcast_ref::<crate::VfsError>() {
        return Some(crate::hybrid_bindings::vfs_error_to_error_code(vfs_err));
    }
    err.downcast_ref::<std::io::Error>()
        .map(crate::hybrid_bindings::io_error_to_error_code)
}

/// The file permissions an `open-at` call asks for.
fn requested_file_perms(flags: types::DescriptorFlags) -> FilePerms {
    let mut perms = FilePerms::empty();
    if flags.contains(types::DescriptorFlags::READ) {
        perms |= FilePerms::READ;
    }
    if flags.contains(types::DescriptorFlags::WRITE) {
        perms |= FilePerms::WRITE;
    }
    perms
}

/// Convert VFS metadata to hybrid WASI DescriptorStat.
fn vfs_metadata_to_stat(meta: &crate::storage::Metadata) -> types::DescriptorStat {
    let dtype = if meta.is_dir {
        types::DescriptorType::Directory
    } else {
        types::DescriptorType::RegularFile
    };

    // Only files store times; directory metadata just carries "now".
    let time = |t| (!meta.is_dir).then(|| crate::wasi_impl::systemtime_to_datetime(t));

    types::DescriptorStat {
        type_: dtype,
        link_count: 1,
        size: meta.size,
        data_access_timestamp: time(meta.accessed),
        data_modification_timestamp: time(meta.modified),
        status_change_timestamp: None,
    }
}

/// Convert a WASI timestamp update to the time to set, if any.
fn new_timestamp(ts: types::NewTimestamp) -> crate::VfsResult<Option<std::time::SystemTime>> {
    match ts {
        types::NewTimestamp::NoChange => Ok(None),
        types::NewTimestamp::Now => Ok(Some(std::time::SystemTime::now())),
        types::NewTimestamp::Timestamp(dt) => {
            crate::wasi_impl::datetime_to_systemtime(dt).map(Some)
        }
    }
}

/// `std::fs::FileTimes` for an optional access/modification time pair.
fn file_times(
    atime: Option<std::time::SystemTime>,
    mtime: Option<std::time::SystemTime>,
) -> std::fs::FileTimes {
    let mut times = std::fs::FileTimes::new();
    if let Some(t) = atime {
        times = times.set_accessed(t);
    }
    if let Some(t) = mtime {
        times = times.set_modified(t);
    }
    times
}

/// Metadata hash for a real file or directory: stable for the same object,
/// and changes when its size or modification time does.
fn real_metadata_hash(guest_path: &str, meta: &capfs::Metadata) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    #[cfg(unix)]
    {
        let _ = guest_path;
        capfs::MetadataExt::dev(meta).hash(&mut hasher);
        capfs::MetadataExt::ino(meta).hash(&mut hasher);
    }
    #[cfg(not(unix))]
    guest_path.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().map(|t| t.into_std()).hash(&mut hasher);
    hasher.finish()
}

/// Metadata used to decide whether two real descriptors are the same object.
#[cfg(unix)]
fn real_metadata(descriptor: &HybridDescriptor) -> Option<capfs::Metadata> {
    match descriptor {
        HybridDescriptor::RealDir { dir, .. } => dir.dir.dir_metadata().ok(),
        HybridDescriptor::RealFile { file, .. } => capfs::Metadata::from_file(&file.file).ok(),
        HybridDescriptor::Vfs(_) => None,
    }
}

/// Convert cap-primitives metadata to hybrid WASI DescriptorStat.
fn cap_metadata_to_stat(meta: &capfs::Metadata) -> types::DescriptorStat {
    let file_type = meta.file_type();
    let dtype = if file_type.is_dir() {
        types::DescriptorType::Directory
    } else if file_type.is_symlink() {
        types::DescriptorType::SymbolicLink
    } else if file_type.is_file() {
        types::DescriptorType::RegularFile
    } else {
        types::DescriptorType::Unknown
    };

    fn to_datetime(t: cap_primitives::time::SystemTime) -> types::Datetime {
        let d = t
            .into_std()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        types::Datetime {
            seconds: d.as_secs(),
            nanoseconds: d.subsec_nanos(),
        }
    }

    let atime = meta.accessed().ok().map(to_datetime);
    let mtime = meta.modified().ok().map(to_datetime);
    let ctime = meta.created().ok().map(to_datetime);

    #[cfg(unix)]
    let link_count = capfs::MetadataExt::nlink(meta);
    #[cfg(not(unix))]
    let link_count = 1;

    types::DescriptorStat {
        type_: dtype,
        link_count,
        size: meta.len(),
        data_access_timestamp: atime,
        data_modification_timestamp: mtime,
        status_change_timestamp: ctime,
    }
}

// ============================================================================
// types::HostDescriptor Implementation
// ============================================================================

// Every operation lives in an inherent method on `HybridVfsState`, shared with
// the WASI 0.3 bindings in `hybrid_p3`. Those run with only brief, synchronous
// access to the store, so an operation that has to await storage is split:
// `*_op` does every descriptor, permission and mount check up front and
// returns an `Op` that owns what it needs (a storage handle and a path) and
// borrows nothing. Operations that never wait are plain `*_now` methods.
//
// On p3, calls on one store can interleave across an op's await, so an op
// must not rely on anything checked in prepare that another call could
// change meanwhile:
// - The storage quota and entry limit are charged inside `VfsStorage`, under
//   its lock, by the mutation itself, so concurrent writes can't both pass.
// - Host files (real preopens) are opened, checked against both open-file
//   limits and tracked in the same synchronous prepare step, never across an
//   await; size caps on them are checked synchronously at each write.
// - An op touches only its cloned storage handle and path. Closing the
//   descriptor meanwhile drops nothing it needs and revives nothing; the only
//   table write afterwards is `open_at` inserting a new, self-contained
//   descriptor.

/// The storage part of an operation, after its checks have passed.
pub(crate) type Op<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = HybridFsResult<T>> + Send>>;

fn op<T>(fut: impl std::future::Future<Output = HybridFsResult<T>> + Send + 'static) -> Op<T> {
    Box::pin(fut)
}

fn done<T: Send + 'static>(value: T) -> Op<T> {
    Box::pin(std::future::ready(Ok(value)))
}

impl<S: VfsStorage + Clone + 'static> types::HostDescriptor for HybridVfsState<'_, S> {
    async fn advise(
        &mut self,
        _fd: Resource<HybridDescriptor>,
        _offset: types::Filesize,
        _len: types::Filesize,
        _advice: types::Advice,
    ) -> HybridFsResult<()> {
        // Advise is a hint - we can ignore it
        Ok(())
    }

    async fn sync_data(&mut self, fd: Resource<HybridDescriptor>) -> HybridFsResult<()> {
        self.sync_data_now(&fd)
    }

    async fn get_flags(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorFlags> {
        self.get_flags_now(&fd)
    }

    async fn get_type(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorType> {
        self.get_type_now(&fd)
    }

    async fn set_size(
        &mut self,
        fd: Resource<HybridDescriptor>,
        size: types::Filesize,
    ) -> HybridFsResult<()> {
        self.set_size_op(&fd, size)?.await
    }

    async fn set_times(
        &mut self,
        fd: Resource<HybridDescriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        self.set_times_op(&fd, atim, mtim)?.await
    }

    async fn read(
        &mut self,
        fd: Resource<HybridDescriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> HybridFsResult<(Vec<u8>, bool)> {
        self.read_op(&fd, len, offset)?.await
    }

    async fn write(
        &mut self,
        fd: Resource<HybridDescriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> HybridFsResult<types::Filesize> {
        self.write_op(&fd, buf, offset)?.await
    }

    async fn read_directory(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<Resource<HybridReaddirIterator>> {
        let entries = self.directory_entries(&fd)?.await?;
        Ok(self.table.push(HybridReaddirIterator::new(entries))?)
    }

    async fn sync(&mut self, fd: Resource<HybridDescriptor>) -> HybridFsResult<()> {
        self.sync_now(&fd)
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        self.create_directory_at_op(&fd, path)?.await
    }

    async fn stat(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorStat> {
        self.stat_op(&fd)?.await
    }

    async fn stat_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::DescriptorStat> {
        self.stat_at_op(&fd, path_flags, path)?.await
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        self.set_times_at_op(&fd, path_flags, path, atim, mtim)?
            .await
    }

    async fn link_at(
        &mut self,
        _fd: Resource<HybridDescriptor>,
        _old_path_flags: types::PathFlags,
        _old_path: String,
        _new_descriptor: Resource<HybridDescriptor>,
        _new_path: String,
    ) -> HybridFsResult<()> {
        link_at_now()
    }

    async fn open_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> HybridFsResult<Resource<HybridDescriptor>> {
        let opened = self
            .open_at_op(&fd, path_flags, path, oflags, flags)?
            .await?;
        Ok(self.table.push(opened)?)
    }

    fn drop(&mut self, fd: Resource<HybridDescriptor>) -> wasmtime::Result<()> {
        self.table.delete(fd)?;
        Ok(())
    }

    async fn readlink_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<String> {
        self.readlink_at_now(&fd, &path)
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        self.remove_directory_at_op(&fd, path)?.await
    }

    async fn rename_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        old_path: String,
        new_fd: Resource<HybridDescriptor>,
        new_path: String,
    ) -> HybridFsResult<()> {
        self.rename_at_op(&fd, old_path, &new_fd, new_path)?.await
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        _src_path: String,
        _dest_path: String,
    ) -> HybridFsResult<()> {
        self.symlink_at_now(&fd)
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        self.unlink_file_at_op(&fd, path)?.await
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> HybridFsResult<Resource<DynInputStream>> {
        let stream = self.input_stream(&fd, offset)?;
        Ok(self.table.push(stream)?)
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> HybridFsResult<Resource<DynOutputStream>> {
        let stream = self.output_stream(&fd, Some(offset))?;
        Ok(self.table.push(stream)?)
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<Resource<DynOutputStream>> {
        let stream = self.output_stream(&fd, None)?;
        Ok(self.table.push(stream)?)
    }

    async fn is_same_object(
        &mut self,
        a: Resource<HybridDescriptor>,
        b: Resource<HybridDescriptor>,
    ) -> wasmtime::Result<bool> {
        self.is_same_object_now(&a, &b)
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::MetadataHashValue> {
        self.metadata_hash_op(&fd)?.await
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::MetadataHashValue> {
        self.metadata_hash_at_op(&fd, path_flags, path)?.await
    }
}

/// Hard links are not supported in the hybrid VFS.
pub(crate) fn link_at_now() -> HybridFsResult<()> {
    Err(crate::VfsError::PermissionDenied("hard links not supported".to_string()).into())
}

impl<S: VfsStorage + Clone + 'static> HybridVfsState<'_, S> {
    pub(crate) fn sync_data_now(&mut self, fd: &Resource<HybridDescriptor>) -> HybridFsResult<()> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => {
                // In-memory storage is always "synced"
                Ok(())
            }
            HybridDescriptor::RealDir { .. } => {
                // Directories don't have sync_data, just return success
                Ok(())
            }
            HybridDescriptor::RealFile { file, .. } => {
                file.file.sync_data().map_err(HybridFsError::from)?;
                Ok(())
            }
        }
    }

    pub(crate) fn get_flags_now(
        &mut self,
        fd: &Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorFlags> {
        let descriptor = self.table.get(fd)?;
        let mut flags = types::DescriptorFlags::empty();

        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.file_perms.contains(FilePerms::READ) {
                    flags |= types::DescriptorFlags::READ;
                }
                if vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    flags |= types::DescriptorFlags::WRITE;
                }
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if dir.file_perms.contains(FilePerms::READ) {
                    flags |= types::DescriptorFlags::READ;
                }
                if dir.file_perms.contains(FilePerms::WRITE) {
                    flags |= types::DescriptorFlags::WRITE;
                }
            }
            HybridDescriptor::RealFile { file, .. } => {
                if file.readable {
                    flags |= types::DescriptorFlags::READ;
                }
                if file.writable {
                    flags |= types::DescriptorFlags::WRITE;
                }
            }
        }

        Ok(flags)
    }

    pub(crate) fn get_type_now(
        &mut self,
        fd: &Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorType> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    Ok(types::DescriptorType::Directory)
                } else {
                    Ok(types::DescriptorType::RegularFile)
                }
            }
            HybridDescriptor::RealDir { .. } => Ok(types::DescriptorType::Directory),
            HybridDescriptor::RealFile { .. } => Ok(types::DescriptorType::RegularFile),
        }
    }

    pub(crate) fn set_size_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        size: types::Filesize,
    ) -> HybridFsResult<Op<()>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.set_size(&path, size).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                file.check_size(size)?;
                file.file.set_len(size)?;
                Ok(done(()))
            }
        }
    }

    pub(crate) fn set_times_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<Op<()>> {
        let atime = new_timestamp(atim)?;
        let mtime = new_timestamp(mtim)?;
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let allowed = if vfs_desc.is_dir {
                    vfs_desc.dir_perms.contains(DirPerms::MUTATE)
                } else {
                    vfs_desc.file_perms.contains(FilePerms::WRITE)
                };
                if !allowed {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.set_times(&path, atime, mtime).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.set_dir_times(file_times(atime, mtime))?;
                Ok(done(()))
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                file.file.set_times(file_times(atime, mtime))?;
                Ok(done(()))
            }
        }
    }

    pub(crate) fn read_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> HybridFsResult<Op<(Vec<u8>, bool)>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                // Reads may be short; never size a buffer by the guest's ask.
                let len = len.min(MAX_READ_CHUNK as u64);
                Ok(op(async move {
                    let data = storage.read_at(&path, offset, len).await?;
                    let eof = (data.len() as u64) < len;
                    Ok((data, eof))
                }))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.readable {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let file_arc = Arc::clone(&file.file);

                // `len` is the guest's ask; reads may be short, so cap the
                // host buffer as the read stream does.
                let len = len.min(MAX_READ_CHUNK as u64) as usize;
                let mut buf = vec![0u8; len];
                let bytes_read = read_at(&file_arc, &mut buf, offset)?;
                buf.truncate(bytes_read);
                let eof = bytes_read < len;
                Ok(done((buf, eof)))
            }
        }
    }

    pub(crate) fn write_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> HybridFsResult<Op<types::Filesize>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    let len = buf.len() as u64;
                    storage.write_at(&path, offset, &buf).await?;
                    Ok(len)
                }))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                file.check_size(offset.saturating_add(buf.len() as u64))?;
                let file_arc = Arc::clone(&file.file);

                Ok(done(write_at(&file_arc, &buf, offset)? as u64))
            }
        }
    }

    pub(crate) fn sync_now(&mut self, fd: &Resource<HybridDescriptor>) -> HybridFsResult<()> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => Ok(()),
            HybridDescriptor::RealDir { .. } => {
                // Directories don't have sync, just return success
                Ok(())
            }
            HybridDescriptor::RealFile { file, .. } => {
                file.file.sync_all().map_err(HybridFsError::from)?;
                Ok(())
            }
        }
    }

    pub(crate) fn create_directory_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<Op<()>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.mkdir(&full_path).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.create_dir(&path)?;
                Ok(done(()))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn stat_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
    ) -> HybridFsResult<Op<types::DescriptorStat>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    let meta = storage.stat(&path).await?;
                    Ok(vfs_metadata_to_stat(&meta))
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let meta = dir.dir.dir_metadata()?;
                Ok(done(cap_metadata_to_stat(&meta)))
            }
            HybridDescriptor::RealFile { file, .. } => {
                let meta = capfs::Metadata::from_file(&file.file)?;
                Ok(done(cap_metadata_to_stat(&meta)))
            }
        }
    }

    pub(crate) fn stat_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<Op<types::DescriptorStat>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    let meta = storage.stat(&full_path).await?;
                    Ok(vfs_metadata_to_stat(&meta))
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let meta = dir.dir.metadata(&path)?;
                Ok(done(cap_metadata_to_stat(&meta)))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn set_times_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<Op<()>> {
        let atime = new_timestamp(atim)?;
        let mtime = new_timestamp(mtim)?;
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.set_times(&full_path, atime, mtime).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let follow = path_flags.contains(types::PathFlags::SYMLINK_FOLLOW);
                dir.dir.set_times(&path, atime, mtime, follow)?;
                Ok(done(()))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    /// Open `path` under `fd`. The returned op resolves to the new
    /// descriptor, which the caller adds to the resource table.
    pub(crate) fn open_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> HybridFsResult<Op<HybridDescriptor>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }

                let full_path = vfs_desc.resolve_path(&path)?;
                let is_directory = oflags.contains(types::OpenFlags::DIRECTORY);

                // The requested file access must be within what the preopen
                // grants.
                let file_perms = requested_file_perms(flags);
                if !is_directory && !vfs_desc.file_perms.contains(file_perms) {
                    return Err(crate::VfsError::AccessDenied(full_path).into());
                }

                let storage = self.ctx.storage_for(vfs_desc).clone();
                let vfs_desc = vfs_desc.clone();
                Ok(op(async move {
                    if is_directory {
                        if !storage.exists(&full_path).await? {
                            return Err(crate::VfsError::NotFound(full_path).into());
                        }
                        let meta = storage.stat(&full_path).await?;
                        if !meta.is_dir {
                            return Err(crate::VfsError::NotDirectory(full_path).into());
                        }
                        let new_vfs_desc = vfs_desc.child_dir(full_path);
                        Ok(HybridDescriptor::Vfs(new_vfs_desc))
                    } else {
                        let create = oflags.contains(types::OpenFlags::CREATE);
                        let exclusive = oflags.contains(types::OpenFlags::EXCLUSIVE);
                        let truncate = oflags.contains(types::OpenFlags::TRUNCATE);

                        // Creating checks and creates in one storage step, so
                        // concurrent `CREATE|EXCLUSIVE` opens (lockfiles) can't
                        // both succeed.
                        let created = if create && vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                            storage.create_file(&full_path, exclusive).await?
                        } else {
                            let exists = storage.exists(&full_path).await?;
                            if exclusive && exists {
                                return Err(crate::VfsError::AlreadyExists(full_path).into());
                            }
                            if !exists {
                                if !create {
                                    return Err(crate::VfsError::NotFound(full_path).into());
                                }
                                return Err(crate::VfsError::PermissionDenied(
                                    "mutate".to_string(),
                                )
                                .into());
                            }
                            false
                        };

                        if !created {
                            let meta = storage.stat(&full_path).await?;
                            if meta.is_dir {
                                return Err(crate::VfsError::NotFile(full_path).into());
                            }
                            if truncate {
                                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                                    return Err(crate::VfsError::PermissionDenied(
                                        "mutate".to_string(),
                                    )
                                    .into());
                                }
                                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                                    return Err(crate::VfsError::AccessDenied(full_path).into());
                                }
                                storage.write(&full_path, &[]).await?;
                            }
                        }

                        let mut new_vfs_desc = VfsDescriptor::file(full_path, file_perms);
                        new_vfs_desc.storage = vfs_desc.storage;
                        Ok(HybridDescriptor::Vfs(new_vfs_desc))
                    }
                }))
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let is_directory = oflags.contains(types::OpenFlags::DIRECTORY);
                let create = oflags.contains(types::OpenFlags::CREATE);
                let exclusive = oflags.contains(types::OpenFlags::EXCLUSIVE);
                let truncate = oflags.contains(types::OpenFlags::TRUNCATE);

                let full_guest_path = format!("{}/{}", guest_path.trim_end_matches('/'), path);

                if is_directory {
                    self.ctx.check_open_host_files()?;
                    // Opening a subdirectory — RestrictedDir::open_dir checks + translates
                    let restricted_sub = dir.dir.open_dir(&path).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound
                            || e.kind() == std::io::ErrorKind::PermissionDenied
                        {
                            crate::VfsError::NotFound(full_guest_path.clone())
                        } else {
                            crate::VfsError::from(e)
                        }
                    })?;
                    self.ctx.track_open_host_file(restricted_sub.handle())?;
                    let new_real_dir = RealDir {
                        dir: restricted_sub,
                        dir_perms: dir.dir_perms,
                        file_perms: dir.file_perms,
                        max_file_size: dir.max_file_size,
                    };
                    Ok(done(HybridDescriptor::RealDir {
                        dir: new_real_dir,
                        guest_path: full_guest_path,
                    }))
                } else {
                    // Opening a file
                    let readable = flags.contains(types::DescriptorFlags::READ);
                    let writable = flags.contains(types::DescriptorFlags::WRITE);

                    // Enforce permission checks from the preopen configuration
                    if !dir.file_perms.contains(requested_file_perms(flags))
                        || (truncate && !dir.file_perms.contains(FilePerms::WRITE))
                    {
                        return Err(crate::VfsError::AccessDenied(full_guest_path).into());
                    }
                    if create && !dir.dir_perms.contains(DirPerms::MUTATE) {
                        return Err(crate::VfsError::PermissionDenied(format!(
                            "create access denied for {}",
                            full_guest_path
                        ))
                        .into());
                    }

                    self.ctx.check_open_host_files()?;
                    let mut open_opts = capfs::OpenOptions::new();
                    open_opts.read(readable);
                    open_opts.write(writable);
                    open_opts.create(create);
                    open_opts.truncate(truncate);
                    if exclusive {
                        open_opts.create_new(true);
                    }

                    // RestrictedDir::open_with checks + translates
                    let file = dir.dir.open_with(&path, &open_opts).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound
                            || e.kind() == std::io::ErrorKind::PermissionDenied
                        {
                            crate::VfsError::NotFound(full_guest_path.clone())
                        } else if e.kind() == std::io::ErrorKind::AlreadyExists {
                            crate::VfsError::AlreadyExists(full_guest_path.clone())
                        } else {
                            crate::VfsError::from(e)
                        }
                    })?;

                    let file = Arc::new(file);
                    let perms = dir.file_perms;
                    let max_size = dir.max_file_size;
                    self.ctx.track_open_host_file(&file)?;
                    let new_real_file = RealFile {
                        file,
                        perms,
                        readable,
                        writable,
                        max_size,
                    };
                    Ok(done(HybridDescriptor::RealFile {
                        file: new_real_file,
                        guest_path: full_guest_path,
                    }))
                }
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn readlink_at_now(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        path: &str,
    ) -> HybridFsResult<String> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => {
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let target = dir.dir.read_link(path)?;
                Ok(target.to_string_lossy().into_owned())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn remove_directory_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<Op<()>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.rmdir(&full_path).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_dir(&path)?;
                Ok(done(()))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn rename_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        old_path: String,
        new_fd: &Resource<HybridDescriptor>,
        new_path: String,
    ) -> HybridFsResult<Op<()>> {
        let old_descriptor = self.table.get(fd)?;
        let new_descriptor = self.table.get(new_fd)?;

        // Both descriptors must be the same type (both VFS or both real)
        match (old_descriptor, new_descriptor) {
            (HybridDescriptor::Vfs(old_vfs), HybridDescriptor::Vfs(new_vfs)) => {
                if !old_vfs.is_dir || !new_vfs.is_dir {
                    return Err(crate::VfsError::NotDirectory("descriptor".to_string()).into());
                }
                if !old_vfs.dir_perms.contains(DirPerms::MUTATE)
                    || !new_vfs.dir_perms.contains(DirPerms::MUTATE)
                {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                if old_vfs.storage != new_vfs.storage {
                    return Err(crate::VfsError::CrossDevice(
                        "rename between storages".to_string(),
                    )
                    .into());
                }
                let old_full = old_vfs.resolve_path(&old_path)?;
                let new_full = new_vfs.resolve_path(&new_path)?;
                let storage = self.ctx.storage_for(old_vfs).clone();
                Ok(op(async move {
                    storage.rename(&old_full, &new_full).await?;
                    Ok(())
                }))
            }
            (
                HybridDescriptor::RealDir { dir: old_dir, .. },
                HybridDescriptor::RealDir { dir: new_dir, .. },
            ) => {
                if !old_dir.dir_perms.contains(DirPerms::MUTATE)
                    || !new_dir.dir_perms.contains(DirPerms::MUTATE)
                {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                // RestrictedDir::rename checks + translates both sides
                old_dir.dir.rename(&old_path, &new_dir.dir, &new_path)?;
                Ok(done(()))
            }
            _ => {
                // Cross-filesystem rename not supported
                Err(crate::VfsError::CrossDevice("rename between filesystems".to_string()).into())
            }
        }
    }

    pub(crate) fn symlink_at_now(&mut self, fd: &Resource<HybridDescriptor>) -> HybridFsResult<()> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => {
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            // Links written into a host directory would outlive the sandbox,
            // and host tools reading it later follow them, so the guest
            // can't create any (as on VFS preopens).
            HybridDescriptor::RealDir { .. } => {
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn unlink_file_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<Op<()>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    storage.delete(&full_path).await?;
                    Ok(())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_file(&path)?;
                Ok(done(()))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    pub(crate) fn is_same_object_now(
        &mut self,
        a: &Resource<HybridDescriptor>,
        b: &Resource<HybridDescriptor>,
    ) -> wasmtime::Result<bool> {
        let desc_a = self.table.get(a)?;
        let desc_b = self.table.get(b)?;
        match (desc_a, desc_b) {
            (HybridDescriptor::Vfs(a), HybridDescriptor::Vfs(b)) => Ok(a.storage == b.storage
                && crate::wasi_impl::vfs_identity(&a.path)
                    == crate::wasi_impl::vfs_identity(&b.path)),
            (HybridDescriptor::Vfs(_), _) | (_, HybridDescriptor::Vfs(_)) => Ok(false),
            #[cfg(unix)]
            _ => Ok(match (real_metadata(desc_a), real_metadata(desc_b)) {
                (Some(a), Some(b)) => {
                    use capfs::MetadataExt;
                    (a.dev(), a.ino()) == (b.dev(), b.ino())
                }
                _ => false,
            }),
            #[cfg(not(unix))]
            _ => Ok(desc_a.path() == desc_b.path()),
        }
    }

    pub(crate) fn metadata_hash_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
    ) -> HybridFsResult<Op<types::MetadataHashValue>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    let meta = storage.stat(&path).await?;
                    let hash = crate::wasi_impl::vfs_metadata_hash(&path, &meta);
                    Ok(types::MetadataHashValue {
                        lower: hash,
                        upper: 0,
                    })
                }))
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.dir_metadata()?;
                let hash = real_metadata_hash(guest_path, &meta);
                Ok(done(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                }))
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                let meta = capfs::Metadata::from_file(&file.file)?;
                let hash = real_metadata_hash(guest_path, &meta);
                Ok(done(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                }))
            }
        }
    }

    pub(crate) fn metadata_hash_at_op(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<Op<types::MetadataHashValue>> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let full_path = vfs_desc.resolve_path(&path)?;
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(op(async move {
                    let meta = storage.stat(&full_path).await?;
                    let hash = crate::wasi_impl::vfs_metadata_hash(&full_path, &meta);
                    Ok(types::MetadataHashValue {
                        lower: hash,
                        upper: 0,
                    })
                }))
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.metadata(&path)?;
                let full_path = format!("{}/{}", guest_path, path);
                let hash = real_metadata_hash(&full_path, &meta);
                Ok(done(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                }))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }
}

// ============================================================================
// Shared by the p2 and p3 bindings
// ============================================================================

/// A pending directory listing, from [`HybridVfsState::directory_entries`].
pub(crate) type DirectoryListing = std::pin::Pin<
    Box<dyn std::future::Future<Output = HybridFsResult<Vec<types::DirectoryEntry>>> + Send>,
>;

impl<S: VfsStorage + Clone + 'static> HybridVfsState<'_, S> {
    /// List the directory `fd`, after its permission checks.
    ///
    /// The checks run now; the returned future does the listing and borrows
    /// nothing, so the p3 bindings can drive it from a stream producer.
    pub(crate) fn directory_entries(
        &self,
        fd: &Resource<HybridDescriptor>,
    ) -> HybridFsResult<DirectoryListing> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(Box::pin(async move {
                    let vfs_entries = storage.list(&path).await?;
                    Ok(vfs_entries
                        .into_iter()
                        .map(|e| types::DirectoryEntry {
                            name: e.name,
                            type_: if e.metadata.is_dir {
                                types::DescriptorType::Directory
                            } else {
                                types::DescriptorType::RegularFile
                            },
                        })
                        .collect())
                }))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }

                // RestrictedDir::entries() handles filtering + RestrictedDir::guest_name()
                // handles the reverse translation — no manual file_map logic needed.
                let raw_entries = dir.dir.entries()?;

                let mut entries = Vec::new();
                for entry in raw_entries {
                    let name = dir.dir.guest_name(&entry);
                    let file_type = entry.file_type()?;
                    let type_ = if file_type.is_dir() {
                        types::DescriptorType::Directory
                    } else if file_type.is_symlink() {
                        types::DescriptorType::SymbolicLink
                    } else if file_type.is_file() {
                        types::DescriptorType::RegularFile
                    } else {
                        types::DescriptorType::Unknown
                    };
                    entries.push(types::DirectoryEntry { name, type_ });
                }

                Ok(Box::pin(std::future::ready(Ok(entries))))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    /// A read stream over the file `fd` from `offset`, after its permission
    /// checks.
    pub(crate) fn input_stream(
        &self,
        fd: &Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> HybridFsResult<DynInputStream> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                let stream = VfsInputStream::new(storage, path, offset);
                Ok(Box::new(stream))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.readable {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let file_arc = Arc::clone(&file.file);
                let stream = RealFileInputStream::new(file_arc, offset);
                Ok(Box::new(stream))
            }
        }
    }

    /// A write stream over the file `fd`, at `offset` or appending when it is
    /// `None`, after its permission checks.
    pub(crate) fn output_stream(
        &mut self,
        fd: &Resource<HybridDescriptor>,
        offset: Option<types::Filesize>,
    ) -> HybridFsResult<DynOutputStream> {
        let descriptor = self.table.get(fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage_for(vfs_desc).clone();
                Ok(Box::new(match offset {
                    Some(offset) => VfsOutputStream::write_at(storage, path, offset),
                    None => VfsOutputStream::append(storage, path),
                }))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let max_size = file.max_size;
                match offset {
                    Some(offset) => Ok(Box::new(RealFileOutputStream::write_at(
                        Arc::clone(&file.file),
                        offset,
                        max_size,
                    ))),
                    None => {
                        // A handle of its own, opened for appending, so the
                        // OS appends atomically even with other writers on
                        // the host file. It counts as an open host file.
                        self.ctx.check_open_host_files()?;
                        let appender = Arc::new(reopen_for_append(&file.file)?);
                        self.ctx.track_open_host_file(&appender)?;
                        Ok(Box::new(RealFileOutputStream::append(appender, max_size)))
                    }
                }
            }
        }
    }
}

// ============================================================================
// types::HostDirectoryEntryStream Implementation
// ============================================================================

impl<S: VfsStorage + Clone + 'static> types::HostDirectoryEntryStream for HybridVfsState<'_, S> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<HybridReaddirIterator>,
    ) -> HybridFsResult<Option<types::DirectoryEntry>> {
        let iterator = self.table.get_mut(&stream)?;
        Ok(iterator.next())
    }

    fn drop(&mut self, stream: Resource<HybridReaddirIterator>) -> wasmtime::Result<()> {
        self.table.delete(stream)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hybrid::HybridVfsCtx;
    use crate::storage::{ArcStorage, InMemoryStorage};
    use types::HostDescriptor;
    use wasmtime::component::ResourceTable;

    fn borrow(fd: &Resource<HybridDescriptor>) -> Resource<HybridDescriptor> {
        Resource::new_borrow(fd.rep())
    }

    fn vfs_err(err: HybridFsError) -> crate::VfsError {
        err.0
    }

    /// Open the preopen at `index`, as the guest's libc does at startup.
    fn preopen(
        state: &mut HybridVfsState<'_, ArcStorage>,
        index: usize,
    ) -> Resource<HybridDescriptor> {
        let mut dirs = preopens::Host::get_directories(state).unwrap();
        dirs.swap_remove(index).0
    }

    async fn open(
        state: &mut HybridVfsState<'_, ArcStorage>,
        dir: &Resource<HybridDescriptor>,
        path: &str,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> HybridFsResult<Resource<HybridDescriptor>> {
        state
            .open_at(
                borrow(dir),
                types::PathFlags::empty(),
                path.to_string(),
                oflags,
                flags,
            )
            .await
    }

    fn storage() -> ArcStorage {
        ArcStorage::new(Arc::new(InMemoryStorage::new()))
    }

    #[tokio::test]
    async fn read_only_file_perms_refuse_write_opens() {
        let mut ctx = HybridVfsCtx::new(storage());
        ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::READ);
        ctx.storages[0].write("/data/f", b"x").await.unwrap();
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let data = preopen(&mut state, 0);

        let write = types::DescriptorFlags::WRITE;
        for oflags in [types::OpenFlags::empty(), types::OpenFlags::CREATE] {
            for path in ["f", "new"] {
                let err = open(&mut state, &data, path, oflags, write).await;
                assert!(
                    matches!(
                        err.map(drop).map_err(vfs_err),
                        Err(crate::VfsError::AccessDenied(_))
                    ),
                    "{path} {oflags:?}"
                );
            }
        }
        let err = open(
            &mut state,
            &data,
            "f",
            types::OpenFlags::TRUNCATE,
            types::DescriptorFlags::READ,
        )
        .await;
        assert!(matches!(
            err.map(drop).map_err(vfs_err),
            Err(crate::VfsError::AccessDenied(_))
        ));
        assert_eq!(state.ctx.storages[0].read("/data/f").await.unwrap(), b"x");

        // File permissions don't apply to opening subdirectories.
        state.ctx.storages[0].mkdir("/data/sub").await.unwrap();
        open(
            &mut state,
            &data,
            "sub",
            types::OpenFlags::DIRECTORY,
            types::DescriptorFlags::READ | types::DescriptorFlags::WRITE,
        )
        .await
        .unwrap();

        open(
            &mut state,
            &data,
            "f",
            types::OpenFlags::empty(),
            types::DescriptorFlags::READ,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn vfs_paths_stay_inside_their_preopen() {
        let mut ctx = HybridVfsCtx::new(storage());
        ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::all());
        ctx.storages[0].write("/outside", b"secret").await.unwrap();
        ctx.storages[0].mkdir("/data/sub").await.unwrap();
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let data = preopen(&mut state, 0);
        let read = types::DescriptorFlags::READ;
        let none = types::OpenFlags::empty();

        for path in ["../outside", "sub/../../outside", "/outside", "/data/sub"] {
            let err = open(&mut state, &data, path, none, read).await;
            assert!(
                matches!(
                    err.map(drop).map_err(vfs_err),
                    Err(crate::VfsError::PermissionDenied(_))
                ),
                "{path}"
            );
        }
        let sub = open(&mut state, &data, "sub", types::OpenFlags::DIRECTORY, read)
            .await
            .unwrap();
        assert!(
            open(&mut state, &sub, "../../outside", none, read)
                .await
                .is_err()
        );
        // Climbing back up to the preopen itself is fine.
        open(&mut state, &sub, "..", types::OpenFlags::DIRECTORY, read)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn storage_preopens_use_their_own_storage() {
        let mut ctx = HybridVfsCtx::new(storage());
        ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::all());
        let mounted = storage();
        mounted.mkdir_sync("/mnt/repo/src").unwrap();
        mounted.write("/mnt/repo/src/a.py", b"a").await.unwrap();
        ctx.add_storage_preopen(
            "/mnt/repo",
            mounted.clone(),
            DirPerms::all(),
            FilePerms::all(),
        );
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let data = preopen(&mut state, 0);
        let repo = preopen(&mut state, 1);
        let read = types::DescriptorFlags::READ;
        let write = types::DescriptorFlags::WRITE;

        // Descriptors opened beneath the mount, however deep, use its storage.
        let src = open(&mut state, &repo, "src", types::OpenFlags::DIRECTORY, read)
            .await
            .unwrap();
        let a = open(&mut state, &src, "a.py", types::OpenFlags::empty(), read)
            .await
            .unwrap();
        let (bytes, _) = state.read(borrow(&a), 10, 0).await.unwrap();
        assert_eq!(bytes, b"a");
        open(&mut state, &src, "b.py", types::OpenFlags::CREATE, write)
            .await
            .unwrap();
        assert!(mounted.exists("/mnt/repo/src/b.py").await.unwrap());
        assert!(
            !state.ctx.storages[0]
                .exists("/mnt/repo/src/b.py")
                .await
                .unwrap()
        );
        open(&mut state, &data, "c", types::OpenFlags::CREATE, write)
            .await
            .unwrap();
        assert!(state.ctx.storages[0].exists("/data/c").await.unwrap());
        assert!(!mounted.exists("/data/c").await.unwrap());

        // Renames stay within a storage.
        let err = state
            .rename_at(borrow(&src), "b.py".into(), borrow(&data), "b.py".into())
            .await;
        assert!(matches!(
            err.map_err(vfs_err),
            Err(crate::VfsError::CrossDevice(_))
        ));
        state
            .rename_at(borrow(&src), "b.py".into(), borrow(&repo), "b.py".into())
            .await
            .unwrap();
        assert!(mounted.exists("/mnt/repo/b.py").await.unwrap());
        assert!(
            !state
                .is_same_object(borrow(&data), borrow(&repo))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn read_only_storage_preopens_refuse_writes() {
        let mut ctx = HybridVfsCtx::new(storage());
        let mounted = storage();
        mounted.mkdir_sync("/mnt").unwrap();
        mounted.write("/mnt/f", b"x").await.unwrap();
        ctx.add_storage_preopen("/mnt", mounted.clone(), DirPerms::READ, FilePerms::READ);
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);

        for (path, oflags) in [
            ("f", types::OpenFlags::empty()),
            ("new", types::OpenFlags::CREATE),
        ] {
            let err = open(
                &mut state,
                &mnt,
                path,
                oflags,
                types::DescriptorFlags::WRITE,
            )
            .await;
            assert!(err.is_err(), "{path}");
        }
        assert!(
            state
                .unlink_file_at(borrow(&mnt), "f".into())
                .await
                .is_err()
        );
        assert_eq!(mounted.read("/mnt/f").await.unwrap(), b"x");
        assert!(!mounted.exists("/mnt/new").await.unwrap());

        // Mounting doesn't create a read-only storage's directory either.
        let empty = storage();
        state
            .ctx
            .add_storage_preopen("/ro", empty.clone(), DirPerms::READ, FilePerms::READ);
        assert!(!empty.exists("/ro").await.unwrap());
    }

    fn real_ctx(
        dir: &std::path::Path,
        real: impl FnOnce(RealDir) -> RealDir,
    ) -> HybridVfsCtx<ArcStorage> {
        let mut ctx = HybridVfsCtx::new(storage());
        let real_dir = RealDir::open_ambient(dir, DirPerms::all(), FilePerms::all()).unwrap();
        ctx.add_real_preopen("/mnt", real(real_dir));
        ctx
    }

    #[tokio::test]
    async fn descriptor_read_caps_its_buffer() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("big"), vec![1u8; MAX_READ_CHUNK + 10]).unwrap();
        let mut ctx = real_ctx(temp.path(), |d| d);
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        let file = open(
            &mut state,
            &mnt,
            "big",
            types::OpenFlags::empty(),
            types::DescriptorFlags::READ,
        )
        .await
        .unwrap();

        // u64::MAX used to be the size of the host buffer.
        let (data, eof) = state.read(borrow(&file), u64::MAX, 0).await.unwrap();
        assert_eq!((data.len(), eof), (MAX_READ_CHUNK, false));
        let (data, eof) = state
            .read(borrow(&file), u64::MAX, MAX_READ_CHUNK as u64)
            .await
            .unwrap();
        assert_eq!((data.len(), eof), (10, true));
    }

    #[tokio::test]
    async fn open_host_files_are_limited() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("f"), b"x").unwrap();
        std::fs::create_dir(temp.path().join("d")).unwrap();
        let mut ctx = real_ctx(temp.path(), |d| d).with_max_open_host_files(2);
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        let read = types::DescriptorFlags::READ;
        let none = types::OpenFlags::empty();

        let first = open(&mut state, &mnt, "f", none, read).await.unwrap();
        let dir = open(&mut state, &mnt, "d", types::OpenFlags::DIRECTORY, read)
            .await
            .unwrap();
        let err = open(&mut state, &mnt, "f", none, read).await;
        assert!(matches!(
            err.map(drop).map_err(vfs_err),
            Err(crate::VfsError::TooManyOpenFiles(_))
        ));

        // A stream keeps the host file open after its descriptor is closed.
        let stream = state.read_via_stream(borrow(&first), 0).unwrap();
        HostDescriptor::drop(&mut state, first).unwrap();
        assert!(open(&mut state, &mnt, "f", none, read).await.is_err());
        state.table.delete(stream).unwrap();
        open(&mut state, &mnt, "f", none, read).await.unwrap();
        HostDescriptor::drop(&mut state, dir).unwrap();
    }

    /// An append stream on a host file holds its own append-mode handle,
    /// which counts against the open-file limit like any other.
    #[tokio::test]
    async fn host_append_streams_count_as_open_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("f"), b"").unwrap();
        let mut ctx = real_ctx(temp.path(), |d| d).with_max_open_host_files(2);
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        let write = types::DescriptorFlags::WRITE;
        let file = open(&mut state, &mnt, "f", types::OpenFlags::empty(), write)
            .await
            .unwrap();

        let stream = state.append_via_stream(borrow(&file)).unwrap();
        assert!(matches!(
            state.append_via_stream(borrow(&file)).map_err(vfs_err),
            Err(crate::VfsError::TooManyOpenFiles(_))
        ));
        let mut out = state.table.delete(stream).unwrap();
        out.blocking_write_and_flush(bytes::Bytes::from_static(b"ab"))
            .await
            .unwrap();
        drop(out);
        let stream = state.append_via_stream(borrow(&file)).unwrap();
        let mut out = state.table.delete(stream).unwrap();
        out.blocking_write_and_flush(bytes::Bytes::from_static(b"cd"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(temp.path().join("f")).unwrap(), b"abcd");
    }

    #[tokio::test]
    async fn open_host_files_are_limited_across_contexts() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("f"), b"x").unwrap();
        let budget = Box::leak(Box::new(crate::hybrid::HostFileBudget::new(2)));
        let read = types::DescriptorFlags::READ;
        let none = types::OpenFlags::empty();

        let mut a = real_ctx(temp.path(), |d| d).with_process_budget(budget);
        let mut a_table = ResourceTable::new();
        let mut a = HybridVfsState::new(&mut a, &mut a_table);
        let a_mnt = preopen(&mut a, 0);
        let mut b = real_ctx(temp.path(), |d| d).with_process_budget(budget);
        let mut b_table = ResourceTable::new();
        let mut b = HybridVfsState::new(&mut b, &mut b_table);
        let b_mnt = preopen(&mut b, 0);

        let first = open(&mut a, &a_mnt, "f", none, read).await.unwrap();
        open(&mut b, &b_mnt, "f", none, read).await.unwrap();
        // Each context is far below its own limit, but together they hold the
        // whole shared budget.
        let err = open(&mut b, &b_mnt, "f", none, read).await;
        assert!(matches!(
            err.map(drop).map_err(vfs_err),
            Err(crate::VfsError::TooManyOpenFiles(_))
        ));
        HostDescriptor::drop(&mut a, first).unwrap();
        open(&mut b, &b_mnt, "f", none, read).await.unwrap();
    }

    #[tokio::test]
    async fn file_mount_honours_max_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("f");
        std::fs::write(&host, b"").unwrap();
        let mut ctx = HybridVfsCtx::new(storage());
        ctx.add_real_file_preopen_path("/mnt/f", &host, DirPerms::all(), FilePerms::all(), Some(4))
            .unwrap();
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        let file = open(
            &mut state,
            &mnt,
            "f",
            types::OpenFlags::empty(),
            types::DescriptorFlags::WRITE,
        )
        .await
        .unwrap();

        assert_eq!(
            state
                .write(borrow(&file), b"1234".to_vec(), 0)
                .await
                .unwrap(),
            4
        );
        assert!(state.write(borrow(&file), b"5".to_vec(), 4).await.is_err());
    }

    #[tokio::test]
    async fn max_file_size_limits_growth() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = real_ctx(temp.path(), |d| d.with_max_file_size(8));
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        let file = open(
            &mut state,
            &mnt,
            "f",
            types::OpenFlags::CREATE,
            types::DescriptorFlags::WRITE,
        )
        .await
        .unwrap();

        let too_large = |r: HybridFsResult<()>| match r.map_err(vfs_err) {
            Err(crate::VfsError::HostIo(e)) => e.kind() == std::io::ErrorKind::FileTooLarge,
            _ => false,
        };
        assert_eq!(
            state
                .write(borrow(&file), b"12345678".to_vec(), 0)
                .await
                .unwrap(),
            8
        );
        assert!(too_large(
            state.write(borrow(&file), b"9".to_vec(), 8).await.map(drop)
        ));
        assert!(too_large(
            state
                .write(borrow(&file), b"9".to_vec(), u64::MAX)
                .await
                .map(drop)
        ));
        assert!(too_large(state.set_size(borrow(&file), 1 << 40).await));
        state.set_size(borrow(&file), 2).await.unwrap();
        assert_eq!(std::fs::read(temp.path().join("f")).unwrap(), b"12");
    }

    #[tokio::test]
    async fn guest_symlinks_are_refused_on_host_directories() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = real_ctx(temp.path(), |d| d);
        let mut table = ResourceTable::new();
        let mut state = HybridVfsState::new(&mut ctx, &mut table);
        let mnt = preopen(&mut state, 0);
        for target in ["/etc/passwd", "../x", "x"] {
            let err = state
                .symlink_at(borrow(&mnt), target.to_string(), "link".to_string())
                .await;
            assert!(err.is_err(), "{target}");
        }
        assert!(!temp.path().join("link").exists());
    }
}
