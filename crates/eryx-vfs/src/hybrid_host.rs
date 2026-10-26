//! Host trait implementations for hybrid VFS.
//!
//! This module implements the WASI filesystem Host traits for HybridVfsState,
//! routing operations to either VFS storage or real filesystem based on
//! the descriptor type.

use std::sync::Arc;

use cap_primitives::fs as capfs;
use wasmtime::component::Resource;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

use crate::HybridReaddirIterator;
use crate::file_io::{read_at, write_at};
use crate::hybrid::{HybridDescriptor, HybridPreopen, HybridVfsState, RealDir, RealFile};
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
                } => {
                    let vfs_desc = VfsDescriptor::dir(guest_path.clone(), *dir_perms, *file_perms);
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
        let err = self.table.get(&err)?;
        if let Some(vfs_err) = err.downcast_ref::<crate::VfsError>() {
            return Ok(Some(crate::hybrid_bindings::vfs_error_to_error_code(
                vfs_err,
            )));
        }
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            let code = crate::hybrid_bindings::io_error_to_error_code(io_err);
            return Ok(Some(code));
        }
        Ok(None)
    }
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
        let descriptor = self.table.get(&fd)?;
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

    async fn get_flags(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorFlags> {
        let descriptor = self.table.get(&fd)?;
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

    async fn get_type(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorType> {
        let descriptor = self.table.get(&fd)?;
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

    async fn set_size(
        &mut self,
        fd: Resource<HybridDescriptor>,
        size: types::Filesize,
    ) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                self.ctx.storage.set_size(&path, size).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let guest_path = guest_path.clone();
                let descriptor = self.table.get_mut(&fd)?;
                if let HybridDescriptor::RealFile { file, .. } = descriptor {
                    file.check_size(size)?;
                    file.file.set_len(size)?;
                    Ok(())
                } else {
                    Err(crate::VfsError::NotFile(guest_path).into())
                }
            }
        }
    }

    async fn set_times(
        &mut self,
        fd: Resource<HybridDescriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        let atime = new_timestamp(atim)?;
        let mtime = new_timestamp(mtim)?;
        let descriptor = self.table.get(&fd)?;
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
                self.ctx.storage.set_times(&path, atime, mtime).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.set_dir_times(file_times(atime, mtime))?;
                Ok(())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                file.file.set_times(file_times(atime, mtime))?;
                Ok(())
            }
        }
    }

    async fn read(
        &mut self,
        fd: Resource<HybridDescriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> HybridFsResult<(Vec<u8>, bool)> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                // Reads may be short; never size a buffer by the guest's ask.
                let len = len.min(MAX_READ_CHUNK as u64);
                let data = self.ctx.storage.read_at(&path, offset, len).await?;
                let eof = (data.len() as u64) < len;
                Ok((data, eof))
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
                Ok((buf, eof))
            }
        }
    }

    async fn write(
        &mut self,
        fd: Resource<HybridDescriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> HybridFsResult<types::Filesize> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let len = buf.len() as u64;
                self.ctx.storage.write_at(&path, offset, &buf).await?;
                Ok(len)
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

                Ok(write_at(&file_arc, &buf, offset)? as u64)
            }
        }
    }

    async fn read_directory(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<Resource<HybridReaddirIterator>> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let vfs_entries = self.ctx.storage.list(&path).await?;
                // Convert VFS entries to hybrid types::DirectoryEntry
                let entries: Vec<types::DirectoryEntry> = vfs_entries
                    .into_iter()
                    .map(|e| types::DirectoryEntry {
                        name: e.name,
                        type_: if e.metadata.is_dir {
                            types::DescriptorType::Directory
                        } else {
                            types::DescriptorType::RegularFile
                        },
                    })
                    .collect();
                let iterator = HybridReaddirIterator::new(entries);
                let resource = self.table.push(iterator)?;
                Ok(resource)
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

                let iterator = HybridReaddirIterator::new(entries);
                let resource = self.table.push(iterator)?;
                Ok(resource)
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn sync(&mut self, fd: Resource<HybridDescriptor>) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
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

    async fn create_directory_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                self.ctx.storage.mkdir(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.create_dir(&path)?;
                Ok(())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn stat(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::DescriptorStat> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let meta = self.ctx.storage.stat(&vfs_desc.path).await?;
                Ok(vfs_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let meta = dir.dir.dir_metadata()?;
                Ok(cap_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealFile { file, .. } => {
                let meta = capfs::Metadata::from_file(&file.file)?;
                Ok(cap_metadata_to_stat(&meta))
            }
        }
    }

    async fn stat_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::DescriptorStat> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                let meta = self.ctx.storage.stat(&full_path).await?;
                Ok(vfs_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let meta = dir.dir.metadata(&path)?;
                Ok(cap_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        let atime = new_timestamp(atim)?;
        let mtime = new_timestamp(mtim)?;
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                self.ctx.storage.set_times(&full_path, atime, mtime).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let follow = path_flags.contains(types::PathFlags::SYMLINK_FOLLOW);
                dir.dir.set_times(&path, atime, mtime, follow)?;
                Ok(())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn link_at(
        &mut self,
        _fd: Resource<HybridDescriptor>,
        _old_path_flags: types::PathFlags,
        _old_path: String,
        _new_descriptor: Resource<HybridDescriptor>,
        _new_path: String,
    ) -> HybridFsResult<()> {
        // Hard links not supported in hybrid VFS
        Err(crate::VfsError::PermissionDenied("hard links not supported".to_string()).into())
    }

    async fn open_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> HybridFsResult<Resource<HybridDescriptor>> {
        let descriptor = self.table.get(&fd)?;
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

                let exists = self.ctx.storage.exists(&full_path).await?;

                if is_directory {
                    if !exists {
                        return Err(crate::VfsError::NotFound(full_path).into());
                    }
                    let meta = self.ctx.storage.stat(&full_path).await?;
                    if !meta.is_dir {
                        return Err(crate::VfsError::NotDirectory(full_path).into());
                    }
                    let new_vfs_desc = vfs_desc.child_dir(full_path);
                    let new_descriptor = HybridDescriptor::Vfs(new_vfs_desc);
                    let resource = self.table.push(new_descriptor)?;
                    Ok(resource)
                } else {
                    let create = oflags.contains(types::OpenFlags::CREATE);
                    let exclusive = oflags.contains(types::OpenFlags::EXCLUSIVE);
                    let truncate = oflags.contains(types::OpenFlags::TRUNCATE);

                    if exclusive && exists {
                        return Err(crate::VfsError::AlreadyExists(full_path).into());
                    }

                    if !exists {
                        if !create {
                            return Err(crate::VfsError::NotFound(full_path).into());
                        }
                        if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                            return Err(
                                crate::VfsError::PermissionDenied("mutate".to_string()).into()
                            );
                        }
                        self.ctx.storage.write(&full_path, &[]).await?;
                    } else {
                        let meta = self.ctx.storage.stat(&full_path).await?;
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
                            self.ctx.storage.write(&full_path, &[]).await?;
                        }
                    }

                    let new_vfs_desc = VfsDescriptor::file(full_path, file_perms);
                    let new_descriptor = HybridDescriptor::Vfs(new_vfs_desc);
                    let resource = self.table.push(new_descriptor)?;
                    Ok(resource)
                }
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
                    let new_descriptor = HybridDescriptor::RealDir {
                        dir: new_real_dir,
                        guest_path: full_guest_path,
                    };
                    let resource = self.table.push(new_descriptor)?;
                    Ok(resource)
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
                    self.ctx.track_open_host_file(&file)?;
                    let new_real_file = RealFile {
                        file,
                        perms: dir.file_perms,
                        readable,
                        writable,
                        max_size: dir.max_file_size,
                    };
                    let new_descriptor = HybridDescriptor::RealFile {
                        file: new_real_file,
                        guest_path: full_guest_path.clone(),
                    };
                    let resource = self.table.push(new_descriptor)?;
                    Ok(resource)
                }
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
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
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => {
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                let target = dir.dir.read_link(&path)?;
                Ok(target.to_string_lossy().into_owned())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                self.ctx.storage.rmdir(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_dir(&path)?;
                Ok(())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn rename_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        old_path: String,
        new_fd: Resource<HybridDescriptor>,
        new_path: String,
    ) -> HybridFsResult<()> {
        let old_descriptor = self.table.get(&fd)?;
        let new_descriptor = self.table.get(&new_fd)?;

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
                let old_full = old_vfs.resolve_path(&old_path)?;
                let new_full = new_vfs.resolve_path(&new_path)?;
                self.ctx.storage.rename(&old_full, &new_full).await?;
                Ok(())
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
                Ok(())
            }
            _ => {
                // Cross-filesystem rename not supported
                Err(crate::VfsError::PermissionDenied(
                    "cross-filesystem rename not supported".to_string(),
                )
                .into())
            }
        }
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        src_path: String,
        dest_path: String,
    ) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(_) => {
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            // Links written into a host directory would outlive the sandbox,
            // and host tools reading it later follow them, so the guest
            // can't create any (as on VFS preopens).
            HybridDescriptor::RealDir { .. } => {
                let _ = (src_path, dest_path);
                Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        path: String,
    ) -> HybridFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if !vfs_desc.is_dir {
                    return Err(crate::VfsError::NotDirectory(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                let full_path = vfs_desc.resolve_path(&path)?;
                self.ctx.storage.delete(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, .. } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_file(&path)?;
                Ok(())
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> HybridFsResult<Resource<DynInputStream>> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage.clone();
                let stream = VfsInputStream::new(storage, path, offset);
                let stream: DynInputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
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
                let stream: DynInputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
            }
        }
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
        offset: types::Filesize,
    ) -> HybridFsResult<Resource<DynOutputStream>> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage.clone();
                let stream = VfsOutputStream::write_at(storage, path, offset);
                let stream: DynOutputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let stream =
                    RealFileOutputStream::write_at(Arc::clone(&file.file), offset, file.max_size);
                let stream: DynOutputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
            }
        }
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<Resource<DynOutputStream>> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                if vfs_desc.is_dir {
                    return Err(crate::VfsError::NotFile(vfs_desc.path.clone()).into());
                }
                if !vfs_desc.file_perms.contains(FilePerms::WRITE) {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }

                let path = vfs_desc.path.clone();
                let storage = self.ctx.storage.clone();
                let stream = VfsOutputStream::append(storage, path);
                let stream: DynOutputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, .. } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let stream = RealFileOutputStream::append(Arc::clone(&file.file), file.max_size);
                let stream: DynOutputStream = Box::new(stream);
                let resource = self.table.push(stream)?;
                Ok(resource)
            }
        }
    }

    async fn is_same_object(
        &mut self,
        a: Resource<HybridDescriptor>,
        b: Resource<HybridDescriptor>,
    ) -> wasmtime::Result<bool> {
        let desc_a = self.table.get(&a)?;
        let desc_b = self.table.get(&b)?;
        match (desc_a, desc_b) {
            (HybridDescriptor::Vfs(a), HybridDescriptor::Vfs(b)) => {
                Ok(crate::wasi_impl::vfs_identity(&a.path)
                    == crate::wasi_impl::vfs_identity(&b.path))
            }
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

    async fn metadata_hash(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::MetadataHashValue> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let meta = self.ctx.storage.stat(&vfs_desc.path).await?;
                let hash = crate::wasi_impl::vfs_metadata_hash(&vfs_desc.path, &meta);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.dir_metadata()?;
                let hash = real_metadata_hash(guest_path, &meta);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                let meta = capfs::Metadata::from_file(&file.file)?;
                let hash = real_metadata_hash(guest_path, &meta);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
        }
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> HybridFsResult<types::MetadataHashValue> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let full_path = vfs_desc.resolve_path(&path)?;
                let meta = self.ctx.storage.stat(&full_path).await?;
                let hash = crate::wasi_impl::vfs_metadata_hash(&full_path, &meta);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.metadata(&path)?;
                let full_path = format!("{}/{}", guest_path, path);
                let hash = real_metadata_hash(&full_path, &meta);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
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
        ctx.storage.write("/data/f", b"x").await.unwrap();
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
        assert_eq!(state.ctx.storage.read("/data/f").await.unwrap(), b"x");

        // File permissions don't apply to opening subdirectories.
        state.ctx.storage.mkdir("/data/sub").await.unwrap();
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
        ctx.storage.write("/outside", b"secret").await.unwrap();
        ctx.storage.mkdir("/data/sub").await.unwrap();
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
