//! VFS types and utilities for WASI filesystem implementation.
//!
//! This module provides the core types used by the WASI filesystem host
//! implementation: descriptors, directory iterators, and error conversion.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::bindings::{DirPerms, FilePerms, types};
use crate::storage::{DirEntry, Metadata, VfsStorage};

/// Context for VFS operations.
#[derive(Debug)]
pub struct VfsCtx<S: VfsStorage + Clone> {
    /// The storage backend.
    pub storage: S,
    /// Preopened directories (path, dir_perms, file_perms).
    pub preopens: Vec<(String, DirPerms, FilePerms)>,
}

impl<S: VfsStorage + Clone> VfsCtx<S> {
    /// Create a new VFS context with the given storage and root preopen.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            preopens: vec![("/".to_string(), DirPerms::all(), FilePerms::all())],
        }
    }

    /// Create a new VFS context with the given storage and no preopens.
    pub fn new_empty(storage: S) -> Self {
        Self {
            storage,
            preopens: Vec::new(),
        }
    }

    /// Add a preopen directory.
    pub fn preopen(&mut self, path: impl Into<String>, dir_perms: DirPerms, file_perms: FilePerms) {
        self.preopens.push((path.into(), dir_perms, file_perms));
    }

    /// Set preopens, replacing any existing ones.
    pub fn set_preopens(&mut self, preopens: Vec<(String, DirPerms, FilePerms)>) {
        self.preopens = preopens;
    }
}

/// A view into the VFS state for WASI trait implementations.
pub struct VfsState<'a, S: VfsStorage + Clone> {
    /// The VFS context.
    pub ctx: &'a mut VfsCtx<S>,
    /// The resource table for managing descriptors.
    pub table: &'a mut wasmtime::component::ResourceTable,
}

impl<S: VfsStorage + Clone> std::fmt::Debug for VfsState<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsState")
            .field("preopens", &self.ctx.preopens.len())
            .finish_non_exhaustive()
    }
}

/// A file or directory descriptor in the VFS.
#[derive(Debug, Clone)]
pub struct VfsDescriptor {
    /// Absolute path within the VFS.
    pub path: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Directory permissions (for directory descriptors).
    pub dir_perms: DirPerms,
    /// File permissions.
    pub file_perms: FilePerms,
}

impl VfsDescriptor {
    /// Create a new directory descriptor.
    pub fn dir(path: impl Into<String>, dir_perms: DirPerms, file_perms: FilePerms) -> Self {
        Self {
            path: path.into(),
            is_dir: true,
            dir_perms,
            file_perms,
        }
    }

    /// Create a new file descriptor.
    pub fn file(path: impl Into<String>, file_perms: FilePerms) -> Self {
        Self {
            path: path.into(),
            is_dir: false,
            dir_perms: DirPerms::empty(),
            file_perms,
        }
    }

    /// Resolve a relative path against this descriptor.
    pub fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.to_string()
        } else if self.path == "/" {
            format!("/{path}")
        } else {
            format!("{}/{path}", self.path)
        }
    }
}

/// Iterator over directory entries.
#[derive(Debug)]
pub struct VfsReaddirIterator {
    entries: Vec<types::DirectoryEntry>,
    index: usize,
}

impl VfsReaddirIterator {
    /// Create a new readdir iterator.
    pub fn new(entries: Vec<DirEntry>) -> Self {
        let converted: Vec<types::DirectoryEntry> = entries
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
        Self {
            entries: converted,
            index: 0,
        }
    }

    /// Get the next entry.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<types::DirectoryEntry> {
        if self.index < self.entries.len() {
            let entry = self.entries[self.index].clone();
            self.index += 1;
            Some(entry)
        } else {
            None
        }
    }
}

/// Convert VfsError to WASI ErrorCode.
pub fn vfs_error_to_error_code(err: &crate::VfsError) -> types::ErrorCode {
    use crate::VfsError;
    match err {
        VfsError::NotFound(_) => types::ErrorCode::NoEntry,
        VfsError::PermissionDenied(_) => types::ErrorCode::NotPermitted,
        VfsError::AlreadyExists(_) => types::ErrorCode::Exist,
        VfsError::NotDirectory(_) => types::ErrorCode::NotDirectory,
        VfsError::NotFile(_) => types::ErrorCode::IsDirectory,
        VfsError::DirectoryNotEmpty(_) => types::ErrorCode::NotEmpty,
        VfsError::InvalidPath(_) => types::ErrorCode::Invalid,
        VfsError::Io(_) => types::ErrorCode::Io,
        // Only real-directory mounts produce host errors; pure VFS never does.
        VfsError::HostIo(_) => types::ErrorCode::Io,
        VfsError::Storage(_) => types::ErrorCode::Io,
        VfsError::InvalidSeek(_) => types::ErrorCode::InvalidSeek,
        VfsError::Busy(_) => types::ErrorCode::Busy,
        VfsError::Unsupported(_) => types::ErrorCode::Unsupported,
        // ENOSPC: indistinguishable from a full disk from the guest's side.
        VfsError::QuotaExceeded(_) => types::ErrorCode::InsufficientSpace,
    }
}

/// Convert SystemTime to WASI datetime.
pub fn systemtime_to_datetime(
    time: SystemTime,
) -> wasmtime_wasi::p2::bindings::clocks::wall_clock::Datetime {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => wasmtime_wasi::p2::bindings::clocks::wall_clock::Datetime {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        },
        Err(_) => wasmtime_wasi::p2::bindings::clocks::wall_clock::Datetime {
            seconds: 0,
            nanoseconds: 0,
        },
    }
}

/// Convert a WASI datetime to `SystemTime`.
pub(crate) fn datetime_to_systemtime(
    dt: wasmtime_wasi::p2::bindings::clocks::wall_clock::Datetime,
) -> crate::VfsResult<SystemTime> {
    UNIX_EPOCH
        .checked_add(std::time::Duration::new(dt.seconds, dt.nanoseconds))
        .ok_or_else(|| crate::VfsError::InvalidPath("timestamp out of range".to_string()))
}

/// The form of a VFS path used to decide whether two descriptors refer to
/// the same object, so `/a/./b` and `/a/b` compare equal.
pub(crate) fn vfs_identity(path: &str) -> String {
    crate::InMemoryStorage::normalize_path(path).unwrap_or_else(|_| path.to_string())
}

/// Metadata hash for a VFS object: stable for the same object, and changes
/// when a file's size or modification time does.
pub(crate) fn vfs_metadata_hash(path: &str, meta: &Metadata) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    vfs_identity(path).hash(&mut hasher);
    meta.is_dir.hash(&mut hasher);
    // Directory metadata carries no stored times or size.
    if !meta.is_dir {
        meta.size.hash(&mut hasher);
        meta.modified.hash(&mut hasher);
    }
    hasher.finish()
}

/// Convert Metadata to DescriptorStat.
pub fn metadata_to_stat(meta: &Metadata) -> types::DescriptorStat {
    types::DescriptorStat {
        type_: if meta.is_dir {
            types::DescriptorType::Directory
        } else {
            types::DescriptorType::RegularFile
        },
        link_count: 1,
        size: meta.size,
        data_access_timestamp: Some(systemtime_to_datetime(meta.accessed)),
        data_modification_timestamp: Some(systemtime_to_datetime(meta.modified)),
        status_change_timestamp: Some(systemtime_to_datetime(meta.modified)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_hash_tracks_identity_and_modification() {
        let meta = Metadata {
            size: 3,
            ..Metadata::default()
        };
        assert_eq!(vfs_identity("/a/./b/"), vfs_identity("/a/b"));
        assert_eq!(
            vfs_metadata_hash("/a/./b", &meta),
            vfs_metadata_hash("/a/b", &meta)
        );

        // Same size, later modification: the hash must change.
        let touched = Metadata {
            modified: meta.modified + std::time::Duration::from_secs(1),
            ..meta.clone()
        };
        assert_ne!(
            vfs_metadata_hash("/a/b", &meta),
            vfs_metadata_hash("/a/b", &touched)
        );
    }
}
