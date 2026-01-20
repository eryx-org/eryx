//! Host trait implementations for VFS.
//!
//! This module implements the WASI filesystem Host traits for our VFS.

use wasmtime::component::Resource;

use crate::bindings::{DirPerms, FilePerms, VfsFsError, VfsFsResult, preopens, types};
use crate::storage::VfsStorage;
use crate::wasi_impl::{
    VfsDescriptor, VfsReaddirIterator, VfsState, metadata_to_stat, vfs_error_to_error_code,
};

// ============================================================================
// preopens::Host Implementation
// ============================================================================

impl<S: VfsStorage + 'static> preopens::Host for VfsState<'_, S> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<VfsDescriptor>, String)>> {
        let mut result = Vec::new();
        for (path, dir_perms, file_perms) in &self.ctx.preopens {
            let descriptor = VfsDescriptor::dir(path.clone(), *dir_perms, *file_perms);
            let resource = self.table.push(descriptor)?;
            result.push((resource, path.clone()));
        }
        Ok(result)
    }
}

// ============================================================================
// types::Host Implementation
// ============================================================================

impl<S: VfsStorage + 'static> types::Host for VfsState<'_, S> {
    fn convert_error_code(&mut self, err: VfsFsError) -> anyhow::Result<types::ErrorCode> {
        err.downcast()
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<anyhow::Error>,
    ) -> anyhow::Result<Option<types::ErrorCode>> {
        let err = self.table.get(&err)?;
        if let Some(vfs_err) = err.downcast_ref::<crate::VfsError>() {
            return Ok(Some(vfs_error_to_error_code(vfs_err)));
        }
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            // Convert io::Error to ErrorCode
            let code = match io_err.kind() {
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
            };
            return Ok(Some(code));
        }
        Ok(None)
    }
}

// ============================================================================
// types::HostDescriptor Implementation
// ============================================================================

impl<S: VfsStorage + 'static> types::HostDescriptor for VfsState<'_, S> {
    async fn advise(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _offset: types::Filesize,
        _len: types::Filesize,
        _advice: types::Advice,
    ) -> VfsFsResult<()> {
        // Advise is a hint - we can ignore it for VFS
        Ok(())
    }

    async fn sync_data(&mut self, _fd: Resource<VfsDescriptor>) -> VfsFsResult<()> {
        // In-memory storage is always "synced"
        Ok(())
    }

    async fn get_flags(
        &mut self,
        fd: Resource<VfsDescriptor>,
    ) -> VfsFsResult<types::DescriptorFlags> {
        let descriptor = self.table.get(&fd)?;
        let mut flags = types::DescriptorFlags::empty();
        if descriptor.file_perms.contains(FilePerms::READ) {
            flags |= types::DescriptorFlags::READ;
        }
        if descriptor.file_perms.contains(FilePerms::WRITE) {
            flags |= types::DescriptorFlags::WRITE;
        }
        Ok(flags)
    }

    async fn get_type(
        &mut self,
        fd: Resource<VfsDescriptor>,
    ) -> VfsFsResult<types::DescriptorType> {
        let descriptor = self.table.get(&fd)?;
        if descriptor.is_dir {
            Ok(types::DescriptorType::Directory)
        } else {
            Ok(types::DescriptorType::RegularFile)
        }
    }

    async fn set_size(
        &mut self,
        fd: Resource<VfsDescriptor>,
        size: types::Filesize,
    ) -> VfsFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        if descriptor.is_dir {
            return Err(crate::VfsError::NotFile(descriptor.path.clone()).into());
        }
        if !descriptor.file_perms.contains(FilePerms::WRITE) {
            return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
        }
        let path = descriptor.path.clone();
        self.ctx.storage.set_size(&path, size).await?;
        Ok(())
    }

    async fn set_times(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _atim: types::NewTimestamp,
        _mtim: types::NewTimestamp,
    ) -> VfsFsResult<()> {
        // Times are not fully supported in our simple VFS
        Ok(())
    }

    async fn read(
        &mut self,
        fd: Resource<VfsDescriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> VfsFsResult<(Vec<u8>, bool)> {
        let descriptor = self.table.get(&fd)?;
        if descriptor.is_dir {
            return Err(crate::VfsError::NotFile(descriptor.path.clone()).into());
        }
        if !descriptor.file_perms.contains(FilePerms::READ) {
            return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
        }
        let path = descriptor.path.clone();
        let data = self.ctx.storage.read_at(&path, offset, len).await?;
        let eof = data.len() < len as usize;
        Ok((data, eof))
    }

    async fn write(
        &mut self,
        fd: Resource<VfsDescriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> VfsFsResult<types::Filesize> {
        let descriptor = self.table.get(&fd)?;
        if descriptor.is_dir {
            return Err(crate::VfsError::NotFile(descriptor.path.clone()).into());
        }
        if !descriptor.file_perms.contains(FilePerms::WRITE) {
            return Err(crate::VfsError::PermissionDenied("write".to_string()).into());
        }
        let path = descriptor.path.clone();
        let len = buf.len() as u64;
        self.ctx.storage.write_at(&path, offset, &buf).await?;
        Ok(len)
    }

    async fn read_directory(
        &mut self,
        fd: Resource<VfsDescriptor>,
    ) -> VfsFsResult<Resource<VfsReaddirIterator>> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }
        if !descriptor.dir_perms.contains(DirPerms::READ) {
            return Err(crate::VfsError::PermissionDenied("read".to_string()).into());
        }
        let path = descriptor.path.clone();
        let entries = self.ctx.storage.list(&path).await?;
        let iterator = VfsReaddirIterator::new(entries);
        let resource = self.table.push(iterator)?;
        Ok(resource)
    }

    async fn sync(&mut self, _fd: Resource<VfsDescriptor>) -> VfsFsResult<()> {
        // In-memory storage is always "synced"
        Ok(())
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        path: String,
    ) -> VfsFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }
        if !descriptor.dir_perms.contains(DirPerms::MUTATE) {
            return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
        }
        let full_path = descriptor.resolve_path(&path);
        self.ctx.storage.mkdir(&full_path).await?;
        Ok(())
    }

    async fn stat(&mut self, fd: Resource<VfsDescriptor>) -> VfsFsResult<types::DescriptorStat> {
        let descriptor = self.table.get(&fd)?;
        let meta = self.ctx.storage.stat(&descriptor.path).await?;
        Ok(metadata_to_stat(&meta))
    }

    async fn stat_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> VfsFsResult<types::DescriptorStat> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }
        let full_path = descriptor.resolve_path(&path);
        let meta = self.ctx.storage.stat(&full_path).await?;
        Ok(metadata_to_stat(&meta))
    }

    async fn set_times_at(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _path_flags: types::PathFlags,
        _path: String,
        _atim: types::NewTimestamp,
        _mtim: types::NewTimestamp,
    ) -> VfsFsResult<()> {
        // Times are not fully supported
        Ok(())
    }

    async fn link_at(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _old_path_flags: types::PathFlags,
        _old_path: String,
        _new_descriptor: Resource<VfsDescriptor>,
        _new_path: String,
    ) -> VfsFsResult<()> {
        // Hard links not supported in VFS
        Err(crate::VfsError::PermissionDenied("hard links not supported".to_string()).into())
    }

    async fn open_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> VfsFsResult<Resource<VfsDescriptor>> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }

        let full_path = descriptor.resolve_path(&path);

        // Check if we're opening a directory
        let is_directory = oflags.contains(types::OpenFlags::DIRECTORY);

        // Determine permissions based on flags
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

        // Check if the path exists
        let exists = self.ctx.storage.exists(&full_path).await?;

        if is_directory {
            // Opening a directory
            if !exists {
                return Err(crate::VfsError::NotFound(full_path).into());
            }
            let meta = self.ctx.storage.stat(&full_path).await?;
            if !meta.is_dir {
                return Err(crate::VfsError::NotDirectory(full_path).into());
            }
            let new_descriptor =
                VfsDescriptor::dir(full_path, descriptor.dir_perms, descriptor.file_perms);
            let resource = self.table.push(new_descriptor)?;
            Ok(resource)
        } else {
            // Opening a file
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
                // Check mutate permission for creating files
                if !descriptor.dir_perms.contains(DirPerms::MUTATE) {
                    return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                }
                // Create the file
                self.ctx.storage.write(&full_path, &[]).await?;
            } else {
                // Check it's not a directory
                let meta = self.ctx.storage.stat(&full_path).await?;
                if meta.is_dir {
                    return Err(crate::VfsError::NotFile(full_path).into());
                }
                if truncate {
                    if !descriptor.dir_perms.contains(DirPerms::MUTATE) {
                        return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
                    }
                    self.ctx.storage.write(&full_path, &[]).await?;
                }
            }

            let new_descriptor = VfsDescriptor::file(full_path, file_perms);
            let resource = self.table.push(new_descriptor)?;
            Ok(resource)
        }
    }

    fn drop(&mut self, fd: Resource<VfsDescriptor>) -> anyhow::Result<()> {
        self.table.delete(fd)?;
        Ok(())
    }

    async fn readlink_at(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _path: String,
    ) -> VfsFsResult<String> {
        // Symlinks not supported in VFS
        Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        path: String,
    ) -> VfsFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }
        if !descriptor.dir_perms.contains(DirPerms::MUTATE) {
            return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
        }
        let full_path = descriptor.resolve_path(&path);
        self.ctx.storage.rmdir(&full_path).await?;
        Ok(())
    }

    async fn rename_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        old_path: String,
        new_fd: Resource<VfsDescriptor>,
        new_path: String,
    ) -> VfsFsResult<()> {
        let old_descriptor = self.table.get(&fd)?;
        let new_descriptor = self.table.get(&new_fd)?;

        if !old_descriptor.is_dir || !new_descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory("descriptor".to_string()).into());
        }
        if !old_descriptor.dir_perms.contains(DirPerms::MUTATE)
            || !new_descriptor.dir_perms.contains(DirPerms::MUTATE)
        {
            return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
        }

        let old_full_path = old_descriptor.resolve_path(&old_path);
        let new_full_path = new_descriptor.resolve_path(&new_path);
        self.ctx
            .storage
            .rename(&old_full_path, &new_full_path)
            .await?;
        Ok(())
    }

    async fn symlink_at(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _src_path: String,
        _dest_path: String,
    ) -> VfsFsResult<()> {
        // Symlinks not supported in VFS
        Err(crate::VfsError::PermissionDenied("symlinks not supported".to_string()).into())
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        path: String,
    ) -> VfsFsResult<()> {
        let descriptor = self.table.get(&fd)?;
        if !descriptor.is_dir {
            return Err(crate::VfsError::NotDirectory(descriptor.path.clone()).into());
        }
        if !descriptor.dir_perms.contains(DirPerms::MUTATE) {
            return Err(crate::VfsError::PermissionDenied("mutate".to_string()).into());
        }
        let full_path = descriptor.resolve_path(&path);
        self.ctx.storage.delete(&full_path).await?;
        Ok(())
    }

    fn read_via_stream(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _offset: types::Filesize,
    ) -> VfsFsResult<Resource<wasmtime_wasi_io::streams::DynInputStream>> {
        // Stream-based I/O not yet implemented for VFS
        // Python's file I/O uses read/write directly, not streams
        Err(crate::VfsError::PermissionDenied("streams not yet supported".to_string()).into())
    }

    fn write_via_stream(
        &mut self,
        _fd: Resource<VfsDescriptor>,
        _offset: types::Filesize,
    ) -> VfsFsResult<Resource<wasmtime_wasi_io::streams::DynOutputStream>> {
        // Stream-based I/O not yet implemented for VFS
        Err(crate::VfsError::PermissionDenied("streams not yet supported".to_string()).into())
    }

    fn append_via_stream(
        &mut self,
        _fd: Resource<VfsDescriptor>,
    ) -> VfsFsResult<Resource<wasmtime_wasi_io::streams::DynOutputStream>> {
        // Stream-based I/O not yet implemented for VFS
        Err(crate::VfsError::PermissionDenied("streams not yet supported".to_string()).into())
    }

    async fn is_same_object(
        &mut self,
        a: Resource<VfsDescriptor>,
        b: Resource<VfsDescriptor>,
    ) -> anyhow::Result<bool> {
        let desc_a = self.table.get(&a)?;
        let desc_b = self.table.get(&b)?;
        Ok(desc_a.path == desc_b.path)
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<VfsDescriptor>,
    ) -> VfsFsResult<types::MetadataHashValue> {
        let descriptor = self.table.get(&fd)?;
        let meta = self.ctx.storage.stat(&descriptor.path).await?;
        // Simple hash based on path and size
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            descriptor.path.hash(&mut hasher);
            meta.size.hash(&mut hasher);
            hasher.finish()
        };
        Ok(types::MetadataHashValue {
            lower: hash,
            upper: 0,
        })
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<VfsDescriptor>,
        _path_flags: types::PathFlags,
        path: String,
    ) -> VfsFsResult<types::MetadataHashValue> {
        let descriptor = self.table.get(&fd)?;
        let full_path = descriptor.resolve_path(&path);
        let meta = self.ctx.storage.stat(&full_path).await?;
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            full_path.hash(&mut hasher);
            meta.size.hash(&mut hasher);
            hasher.finish()
        };
        Ok(types::MetadataHashValue {
            lower: hash,
            upper: 0,
        })
    }
}

// ============================================================================
// types::HostDirectoryEntryStream Implementation
// ============================================================================

impl<S: VfsStorage + 'static> types::HostDirectoryEntryStream for VfsState<'_, S> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<VfsReaddirIterator>,
    ) -> VfsFsResult<Option<types::DirectoryEntry>> {
        let iterator = self.table.get_mut(&stream)?;
        Ok(iterator.next())
    }

    fn drop(&mut self, stream: Resource<VfsReaddirIterator>) -> anyhow::Result<()> {
        self.table.delete(stream)?;
        Ok(())
    }
}
