//! WASI filesystem bindings for hybrid VFS.
//!
//! This module generates bindings for the `wasi:filesystem` interfaces with
//! our hybrid descriptor type that can route to either VFS or real filesystem.

// Re-export the HybridDescriptor from the hybrid module
pub use crate::hybrid::HybridDescriptor;

/// Error type for hybrid VFS filesystem operations.
#[derive(Debug)]
pub struct HybridFsError(pub crate::VfsError);

impl std::fmt::Display for HybridFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for HybridFsError {}

impl From<crate::VfsError> for HybridFsError {
    fn from(err: crate::VfsError) -> Self {
        HybridFsError(err)
    }
}

impl From<wasmtime::component::ResourceTableError> for HybridFsError {
    fn from(err: wasmtime::component::ResourceTableError) -> Self {
        HybridFsError(crate::VfsError::Io(err.to_string()))
    }
}

impl From<std::io::Error> for HybridFsError {
    fn from(err: std::io::Error) -> Self {
        HybridFsError(crate::VfsError::Io(err.to_string()))
    }
}

/// Iterator for hybrid directory entries.
pub struct HybridReaddirIterator {
    entries: std::vec::IntoIter<types::DirectoryEntry>,
}

impl std::fmt::Debug for HybridReaddirIterator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridReaddirIterator")
            .field("remaining", &self.entries.len())
            .finish()
    }
}

impl HybridReaddirIterator {
    /// Create a new iterator from a list of directory entries.
    pub fn new(entries: Vec<types::DirectoryEntry>) -> Self {
        Self {
            entries: entries.into_iter(),
        }
    }

    /// Get the next directory entry.
    pub fn next(&mut self) -> Option<types::DirectoryEntry> {
        self.entries.next()
    }
}

impl HybridFsError {
    /// Convert to WASI error code.
    pub fn downcast(self) -> anyhow::Result<types::ErrorCode> {
        Ok(vfs_error_to_error_code(&self.0))
    }
}

/// Convert VfsError to WASI ErrorCode.
pub fn vfs_error_to_error_code(err: &crate::VfsError) -> types::ErrorCode {
    match err {
        crate::VfsError::NotFound(_) => types::ErrorCode::NoEntry,
        crate::VfsError::AlreadyExists(_) => types::ErrorCode::Exist,
        crate::VfsError::NotDirectory(_) => types::ErrorCode::NotDirectory,
        crate::VfsError::NotFile(_) => types::ErrorCode::IsDirectory,
        crate::VfsError::DirectoryNotEmpty(_) => types::ErrorCode::NotEmpty,
        crate::VfsError::PermissionDenied(_) => types::ErrorCode::NotPermitted,
        crate::VfsError::InvalidPath(_) => types::ErrorCode::Invalid,
        crate::VfsError::Io(_) => types::ErrorCode::Io,
        crate::VfsError::Storage(_) => types::ErrorCode::Io,
        crate::VfsError::InvalidSeek(_) => types::ErrorCode::Invalid,
        crate::VfsError::Busy(_) => types::ErrorCode::Busy,
    }
}

mod generated {
    pub use super::{HybridDescriptor, HybridFsError, HybridReaddirIterator};

    wasmtime::component::bindgen!({
        path: "wit",
        world: "virtual-filesystem",
        trappable_error_type: {
            "wasi:filesystem/types.error-code" => HybridFsError,
        },
        with: {
            // Map to our hybrid descriptor type
            "wasi:filesystem/types.descriptor": HybridDescriptor,
            "wasi:filesystem/types.directory-entry-stream": HybridReaddirIterator,
            // Use wasmtime-wasi-io for stream types
            "wasi:io/poll": wasmtime_wasi_io::bindings::wasi::io::poll,
            "wasi:io/streams": wasmtime_wasi_io::bindings::wasi::io::streams,
            "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
            // Use wasmtime-wasi for clocks
            "wasi:clocks/wall-clock": wasmtime_wasi::p2::bindings::clocks::wall_clock,
        },
        imports: {
            "wasi:filesystem/types.[method]descriptor.advise": async | trappable,
            "wasi:filesystem/types.[method]descriptor.create-directory-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.get-flags": async | trappable,
            "wasi:filesystem/types.[method]descriptor.get-type": async | trappable,
            "wasi:filesystem/types.[method]descriptor.is-same-object": async | trappable,
            "wasi:filesystem/types.[method]descriptor.link-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.metadata-hash": async | trappable,
            "wasi:filesystem/types.[method]descriptor.metadata-hash-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.open-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.read": async | trappable,
            "wasi:filesystem/types.[method]descriptor.read-directory": async | trappable,
            "wasi:filesystem/types.[method]descriptor.readlink-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.remove-directory-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.rename-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.set-size": async | trappable,
            "wasi:filesystem/types.[method]descriptor.set-times": async | trappable,
            "wasi:filesystem/types.[method]descriptor.set-times-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.stat": async | trappable,
            "wasi:filesystem/types.[method]descriptor.stat-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.symlink-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.sync": async | trappable,
            "wasi:filesystem/types.[method]descriptor.sync-data": async | trappable,
            "wasi:filesystem/types.[method]descriptor.unlink-file-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.write": async | trappable,
            "wasi:filesystem/types.[method]directory-entry-stream.read-directory-entry": async | trappable,
            default: trappable,
        },
        require_store_data_send: true,
    });
}

pub use generated::wasi::filesystem::{preopens, types};

/// Result type for hybrid VFS filesystem operations.
pub type HybridFsResult<T> = Result<T, HybridFsError>;

// Re-export wasmtime-wasi types that we need
pub use wasmtime_wasi::{DirPerms, FilePerms};

use wasmtime::component::HasData;

use crate::hybrid::HybridVfsState;
use crate::storage::VfsStorage;

/// Marker type for implementing filesystem Host traits via HybridVfsState.
pub struct HybridFilesystem<S: VfsStorage + 'static>(std::marker::PhantomData<S>);

impl<S: VfsStorage + 'static> HasData for HybridFilesystem<S> {
    type Data<'a> = HybridVfsState<'a, S>;
}
