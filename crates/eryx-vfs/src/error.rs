//! VFS error types.

use thiserror::Error;

/// Result type for VFS operations.
pub type VfsResult<T> = Result<T, VfsError>;

/// Errors that can occur during VFS operations.
#[derive(Debug, Error)]
pub enum VfsError {
    /// File or directory not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// File already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// Not a directory.
    #[error("not a directory: {0}")]
    NotDirectory(String),

    /// Not a file.
    #[error("not a file: {0}")]
    NotFile(String),

    /// Directory not empty.
    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),

    /// Invalid path.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(String),

    /// Storage backend error.
    #[error("storage error: {0}")]
    Storage(String),

    /// Invalid seek position.
    #[error("invalid seek: {0}")]
    InvalidSeek(String),

    /// Resource busy.
    #[error("resource busy: {0}")]
    Busy(String),
}
