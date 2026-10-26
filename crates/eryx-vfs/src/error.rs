//! VFS error types.

use thiserror::Error;

/// Result type for VFS operations.
pub type VfsResult<T> = Result<T, VfsError>;

/// Errors that can occur during VFS operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VfsError {
    /// File or directory not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The descriptor's permissions don't allow the requested access, such as
    /// opening a file for writing beneath a read-only mount. Guests see
    /// `EACCES`.
    #[error("access denied: {0}")]
    AccessDenied(String),

    /// A path, or one of its components, is longer than the filesystem
    /// accepts. Guests see `ENAMETOOLONG`.
    #[error("name too long: {0}")]
    NameTooLong(String),

    /// The sandbox already holds as many open host files and directories as
    /// it is allowed.
    ///
    /// WASI has no `EMFILE`, so guests see the closest code, `EDQUOT`.
    #[error("too many open files: {0}")]
    TooManyOpenFiles(String),

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

    /// I/O error from the host filesystem behind a real-directory mount.
    ///
    /// Keeps the original error so its kind (not found, permission denied,
    /// ...) reaches the guest as the matching error code rather than `EIO`.
    #[error("I/O error: {0}")]
    HostIo(#[from] std::io::Error),

    /// Storage backend error.
    #[error("storage error: {0}")]
    Storage(String),

    /// Invalid seek position.
    #[error("invalid seek: {0}")]
    InvalidSeek(String),

    /// The storage backend doesn't support this operation.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Resource busy.
    #[error("resource busy: {0}")]
    Busy(String),

    /// The operation would grow the filesystem past its size limit.
    ///
    /// In-memory storage keeps file contents in host memory, so an unbounded
    /// write is an unbounded host allocation. Guests see this as `ENOSPC`
    /// (`OSError` in Python), the same as a full disk.
    #[error("filesystem full: {0}")]
    QuotaExceeded(String),
}
