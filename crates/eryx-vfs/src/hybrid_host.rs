//! Host trait implementations for hybrid VFS.
//!
//! This module implements the WASI filesystem Host traits for HybridVfsState,
//! routing operations to either VFS storage or real filesystem based on
//! the descriptor type.

use std::sync::Arc;

use cap_fs_ext::MetadataExt as CapMetadataExt;
use system_interface::fs::FileIoExt;
use wasmtime::component::Resource;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

use crate::HybridReaddirIterator;
use crate::hybrid::{HybridDescriptor, HybridPreopen, HybridVfsState, RealDir, RealFile};
use crate::hybrid_bindings::{DirPerms, FilePerms, HybridFsError, HybridFsResult, preopens, types};
use crate::storage::VfsStorage;
use crate::streams::{RealFileInputStream, RealFileOutputStream, VfsInputStream, VfsOutputStream};
use crate::wasi_impl::VfsDescriptor;

// ============================================================================
// preopens::Host Implementation
// ============================================================================

impl<S: VfsStorage + 'static> preopens::Host for HybridVfsState<'_, S> {
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

impl<S: VfsStorage + 'static> types::Host for HybridVfsState<'_, S> {
    fn convert_error_code(&mut self, err: HybridFsError) -> anyhow::Result<types::ErrorCode> {
        err.downcast()
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<anyhow::Error>,
    ) -> anyhow::Result<Option<types::ErrorCode>> {
        let err = self.table.get(&err)?;
        if let Some(vfs_err) = err.downcast_ref::<crate::VfsError>() {
            return Ok(Some(crate::hybrid_bindings::vfs_error_to_error_code(
                vfs_err,
            )));
        }
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            let code = io_error_to_error_code(io_err);
            return Ok(Some(code));
        }
        Ok(None)
    }
}

/// Convert std::io::Error to WASI ErrorCode.
fn io_error_to_error_code(err: &std::io::Error) -> types::ErrorCode {
    match err.kind() {
        std::io::ErrorKind::NotFound => types::ErrorCode::NoEntry,
        std::io::ErrorKind::PermissionDenied => types::ErrorCode::NotPermitted,
        std::io::ErrorKind::AlreadyExists => types::ErrorCode::Exist,
        std::io::ErrorKind::WouldBlock => types::ErrorCode::WouldBlock,
        std::io::ErrorKind::InvalidInput => types::ErrorCode::Invalid,
        std::io::ErrorKind::InvalidData => types::ErrorCode::Invalid,
        std::io::ErrorKind::Interrupted => types::ErrorCode::Interrupted,
        std::io::ErrorKind::NotADirectory => types::ErrorCode::NotDirectory,
        std::io::ErrorKind::IsADirectory => types::ErrorCode::IsDirectory,
        std::io::ErrorKind::DirectoryNotEmpty => types::ErrorCode::NotEmpty,
        std::io::ErrorKind::ReadOnlyFilesystem => types::ErrorCode::ReadOnly,
        std::io::ErrorKind::FileTooLarge => types::ErrorCode::FileTooLarge,
        std::io::ErrorKind::ResourceBusy => types::ErrorCode::Busy,
        _ => types::ErrorCode::Io,
    }
}

/// Convert VFS metadata to hybrid WASI DescriptorStat.
fn vfs_metadata_to_stat(meta: &crate::storage::Metadata) -> types::DescriptorStat {
    let dtype = if meta.is_dir {
        types::DescriptorType::Directory
    } else {
        types::DescriptorType::RegularFile
    };

    types::DescriptorStat {
        type_: dtype,
        link_count: 1,
        size: meta.size,
        data_access_timestamp: None,
        data_modification_timestamp: None,
        status_change_timestamp: None,
    }
}

/// Convert cap-std metadata to hybrid WASI DescriptorStat.
fn cap_metadata_to_stat(meta: &cap_std::fs::Metadata) -> types::DescriptorStat {
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

    // Get timestamps using cap-std's SystemTime
    let atime = meta.accessed().ok().map(|t| {
        let d = t
            .into_std()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        types::Datetime {
            seconds: d.as_secs(),
            nanoseconds: d.subsec_nanos(),
        }
    });

    let mtime = meta.modified().ok().map(|t| {
        let d = t
            .into_std()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        types::Datetime {
            seconds: d.as_secs(),
            nanoseconds: d.subsec_nanos(),
        }
    });

    let ctime = meta.created().ok().map(|t| {
        let d = t
            .into_std()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        types::Datetime {
            seconds: d.as_secs(),
            nanoseconds: d.subsec_nanos(),
        }
    });

    types::DescriptorStat {
        type_: dtype,
        link_count: meta.nlink(),
        size: meta.len(),
        data_access_timestamp: atime,
        data_modification_timestamp: mtime,
        status_change_timestamp: ctime,
    }
}

// ============================================================================
// types::HostDescriptor Implementation
// ============================================================================

impl<S: VfsStorage + 'static> types::HostDescriptor for HybridVfsState<'_, S> {
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
                    file.file
                        .set_len(size)
                        .map_err(|e| crate::VfsError::Io(e.to_string()))?;
                    Ok(())
                } else {
                    Err(crate::VfsError::NotFile(guest_path).into())
                }
            }
        }
    }

    async fn set_times(
        &mut self,
        _fd: Resource<HybridDescriptor>,
        _atim: types::NewTimestamp,
        _mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        // Times are not fully supported
        Ok(())
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
                let data = self.ctx.storage.read_at(&path, offset, len).await?;
                let eof = data.len() < len as usize;
                Ok((data, eof))
            }
            HybridDescriptor::RealDir { guest_path, .. } => {
                Err(crate::VfsError::NotFile(guest_path.clone()).into())
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                if !file.readable {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let file_arc = Arc::clone(&file.file);
                let guest_path = guest_path.clone();

                // Read from the file at the specified offset using FileIoExt
                let mut buf = vec![0u8; len as usize];
                let bytes_read = match file_arc.read_at(&mut buf, offset) {
                    Ok(n) => n,
                    Err(e) => {
                        return Err(
                            crate::VfsError::Io(format!("read {}: {}", guest_path, e)).into()
                        );
                    }
                };
                buf.truncate(bytes_read);
                let eof = bytes_read < len as usize;
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
            HybridDescriptor::RealFile { file, guest_path } => {
                if !file.writable {
                    return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
                }
                let file_arc = Arc::clone(&file.file);
                let guest_path = guest_path.clone();

                // Write to the file at the specified offset using FileIoExt
                match file_arc.write_at(&buf, offset) {
                    Ok(n) => Ok(n as u64),
                    Err(e) => {
                        Err(crate::VfsError::Io(format!("write {}: {}", guest_path, e)).into())
                    }
                }
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
            HybridDescriptor::RealDir { dir, guest_path } => {
                if !dir.dir_perms.contains(DirPerms::READ) {
                    return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
                }
                let dir_arc = Arc::clone(&dir.dir);

                // Read directory entries from real filesystem
                let mut entries = Vec::new();
                for entry_result in dir_arc
                    .entries()
                    .map_err(|e| crate::VfsError::Io(format!("read_dir {}: {}", guest_path, e)))?
                {
                    let entry = entry_result
                        .map_err(|e| crate::VfsError::Io(format!("read_dir entry: {}", e)))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let file_type = entry
                        .file_type()
                        .map_err(|e| crate::VfsError::Io(format!("file_type: {}", e)))?;
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
                let full_path = vfs_desc.resolve_path(&path);
                self.ctx.storage.mkdir(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.create_dir(&path).map_err(|e| {
                    crate::VfsError::Io(format!("create_dir {}/{}: {}", guest_path, path, e))
                })?;
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
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir
                    .dir
                    .dir_metadata()
                    .map_err(|e| crate::VfsError::Io(format!("stat {}: {}", guest_path, e)))?;
                Ok(cap_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                let meta = file
                    .file
                    .metadata()
                    .map_err(|e| crate::VfsError::Io(format!("stat {}: {}", guest_path, e)))?;
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
                let full_path = vfs_desc.resolve_path(&path);
                let meta = self.ctx.storage.stat(&full_path).await?;
                Ok(vfs_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.metadata(&path).map_err(|e| {
                    crate::VfsError::Io(format!("stat {}/{}: {}", guest_path, path, e))
                })?;
                Ok(cap_metadata_to_stat(&meta))
            }
            HybridDescriptor::RealFile { guest_path, .. } => {
                Err(crate::VfsError::NotDirectory(guest_path.clone()).into())
            }
        }
    }

    async fn set_times_at(
        &mut self,
        _fd: Resource<HybridDescriptor>,
        _path_flags: types::PathFlags,
        _path: String,
        _atim: types::NewTimestamp,
        _mtim: types::NewTimestamp,
    ) -> HybridFsResult<()> {
        // Times are not fully supported
        Ok(())
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

                let full_path = vfs_desc.resolve_path(&path);
                let is_directory = oflags.contains(types::OpenFlags::DIRECTORY);

                let file_perms = {
                    let mut perms = FilePerms::empty();
                    if flags.contains(types::DescriptorFlags::READ) {
                        perms |= FilePerms::READ;
                    }
                    if flags.contains(types::DescriptorFlags::WRITE) {
                        perms |= FilePerms::WRITE;
                    }
                    perms
                };

                let exists = self.ctx.storage.exists(&full_path).await?;

                if is_directory {
                    if !exists {
                        return Err(crate::VfsError::NotFound(full_path).into());
                    }
                    let meta = self.ctx.storage.stat(&full_path).await?;
                    if !meta.is_dir {
                        return Err(crate::VfsError::NotDirectory(full_path).into());
                    }
                    let new_vfs_desc =
                        VfsDescriptor::dir(full_path, vfs_desc.dir_perms, vfs_desc.file_perms);
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
                    // Opening a subdirectory
                    let sub_dir = dir.dir.open_dir(&path).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            crate::VfsError::NotFound(full_guest_path.clone())
                        } else {
                            crate::VfsError::Io(format!("open_dir {}: {}", full_guest_path, e))
                        }
                    })?;
                    let new_real_dir = RealDir {
                        dir: Arc::new(sub_dir),
                        dir_perms: dir.dir_perms,
                        file_perms: dir.file_perms,
                        allow_blocking: dir.allow_blocking,
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

                    let mut open_opts = cap_std::fs::OpenOptions::new();
                    open_opts.read(readable);
                    open_opts.write(writable);
                    open_opts.create(create);
                    open_opts.truncate(truncate);
                    if exclusive {
                        open_opts.create_new(true);
                    }

                    let file = dir.dir.open_with(&path, &open_opts).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            crate::VfsError::NotFound(full_guest_path.clone())
                        } else if e.kind() == std::io::ErrorKind::AlreadyExists {
                            crate::VfsError::AlreadyExists(full_guest_path.clone())
                        } else {
                            crate::VfsError::Io(format!("open {}: {}", full_guest_path, e))
                        }
                    })?;

                    let new_real_file = RealFile {
                        file: Arc::new(file),
                        perms: dir.file_perms,
                        readable,
                        writable,
                        allow_blocking: dir.allow_blocking,
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

    fn drop(&mut self, fd: Resource<HybridDescriptor>) -> anyhow::Result<()> {
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
            HybridDescriptor::RealDir { dir, guest_path } => {
                let target = dir.dir.read_link(&path).map_err(|e| {
                    crate::VfsError::Io(format!("readlink {}/{}: {}", guest_path, path, e))
                })?;
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
                let full_path = vfs_desc.resolve_path(&path);
                self.ctx.storage.rmdir(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_dir(&path).map_err(|e| {
                    crate::VfsError::Io(format!("rmdir {}/{}: {}", guest_path, path, e))
                })?;
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
                let old_full = old_vfs.resolve_path(&old_path);
                let new_full = new_vfs.resolve_path(&new_path);
                self.ctx.storage.rename(&old_full, &new_full).await?;
                Ok(())
            }
            (
                HybridDescriptor::RealDir {
                    dir: old_dir,
                    guest_path: old_guest,
                },
                HybridDescriptor::RealDir {
                    dir: new_dir,
                    guest_path: new_guest,
                },
            ) => {
                if !old_dir.dir_perms.contains(DirPerms::MUTATE)
                    || !new_dir.dir_perms.contains(DirPerms::MUTATE)
                {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                old_dir
                    .dir
                    .rename(&old_path, &*new_dir.dir, &new_path)
                    .map_err(|e| {
                        crate::VfsError::Io(format!(
                            "rename {}/{} -> {}/{}: {}",
                            old_guest, old_path, new_guest, new_path, e
                        ))
                    })?;
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
            HybridDescriptor::RealDir { dir, guest_path } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir
                    .symlink(&src_path, &dest_path)
                    .map_err(|e| crate::VfsError::Io(format!("symlink {}: {}", guest_path, e)))?;
                Ok(())
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
                let full_path = vfs_desc.resolve_path(&path);
                self.ctx.storage.delete(&full_path).await?;
                Ok(())
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                if !dir.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                dir.dir.remove_file(&path).map_err(|e| {
                    crate::VfsError::Io(format!("unlink {}/{}: {}", guest_path, path, e))
                })?;
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
                let storage = Arc::clone(&self.ctx.storage);
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
                let storage = Arc::clone(&self.ctx.storage);
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
                let stream = RealFileOutputStream::write_at(Arc::clone(&file.file), offset);
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
                // For append, we need to get the current file size first
                // We'll use a large offset to trigger append behavior
                // TODO: Implement proper append stream for VFS
                let path = vfs_desc.path.clone();
                let storage = Arc::clone(&self.ctx.storage);
                // Use u64::MAX as position to indicate append mode
                // The VfsOutputStream will need to handle this
                let stream = VfsOutputStream::write_at(storage, path, u64::MAX);
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
                let stream = RealFileOutputStream::append(Arc::clone(&file.file));
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
    ) -> anyhow::Result<bool> {
        let desc_a = self.table.get(&a)?;
        let desc_b = self.table.get(&b)?;
        Ok(desc_a.path() == desc_b.path())
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<HybridDescriptor>,
    ) -> HybridFsResult<types::MetadataHashValue> {
        let descriptor = self.table.get(&fd)?;
        match descriptor {
            HybridDescriptor::Vfs(vfs_desc) => {
                let meta = self.ctx.storage.stat(&vfs_desc.path).await?;
                let hash = compute_hash(&vfs_desc.path, meta.size);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir
                    .dir
                    .dir_metadata()
                    .map_err(|e| crate::VfsError::Io(format!("stat {}: {}", guest_path, e)))?;
                let hash = compute_hash(guest_path, meta.len());
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealFile { file, guest_path } => {
                let meta = file
                    .file
                    .metadata()
                    .map_err(|e| crate::VfsError::Io(format!("stat {}: {}", guest_path, e)))?;
                let hash = compute_hash(guest_path, meta.len());
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
                let full_path = vfs_desc.resolve_path(&path);
                let meta = self.ctx.storage.stat(&full_path).await?;
                let hash = compute_hash(&full_path, meta.size);
                Ok(types::MetadataHashValue {
                    lower: hash,
                    upper: 0,
                })
            }
            HybridDescriptor::RealDir { dir, guest_path } => {
                let meta = dir.dir.metadata(&path).map_err(|e| {
                    crate::VfsError::Io(format!("stat {}/{}: {}", guest_path, path, e))
                })?;
                let full_path = format!("{}/{}", guest_path, path);
                let hash = compute_hash(&full_path, meta.len());
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

/// Compute a simple hash from path and size.
fn compute_hash(path: &str, size: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    size.hash(&mut hasher);
    hasher.finish()
}

// ============================================================================
// types::HostDirectoryEntryStream Implementation
// ============================================================================

impl<S: VfsStorage + 'static> types::HostDirectoryEntryStream for HybridVfsState<'_, S> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<HybridReaddirIterator>,
    ) -> HybridFsResult<Option<types::DirectoryEntry>> {
        let iterator = self.table.get_mut(&stream)?;
        Ok(iterator.next())
    }

    fn drop(&mut self, stream: Resource<HybridReaddirIterator>) -> anyhow::Result<()> {
        self.table.delete(stream)?;
        Ok(())
    }
}
