//! WASI filesystem bindings for pure VFS.
//!
//! This module generates bindings for the `wasi:filesystem` interfaces with
//! our custom VFS types instead of the default wasmtime-wasi types.
//!
//! ## When to use this vs `hybrid_bindings`
//!
//! - Use **this module** (`bindings`) when you want a pure in-memory VFS with no
//!   real filesystem access. All paths are handled by the `VfsStorage` backend.
//!   Good for fully sandboxed environments where no host filesystem access is needed.
//!
//! - Use **`hybrid_bindings`** when you need to mix VFS storage with real filesystem
//!   passthrough (e.g., `/data/*` goes to VFS, `/python-stdlib/*` goes to real FS).
//!   This is what `SessionExecutor` uses to allow Python stdlib access while
//!   keeping user data sandboxed.

// Import our types that will be used by bindgen
pub use crate::wasi_impl::{VfsDescriptor, VfsReaddirIterator};

/// Error type for VFS filesystem operations.
#[derive(Debug)]
pub struct VfsFsError(pub crate::VfsError);

impl std::fmt::Display for VfsFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for VfsFsError {}

impl From<crate::VfsError> for VfsFsError {
    fn from(err: crate::VfsError) -> Self {
        VfsFsError(err)
    }
}

impl From<wasmtime::component::ResourceTableError> for VfsFsError {
    fn from(err: wasmtime::component::ResourceTableError) -> Self {
        VfsFsError(crate::VfsError::Io(err.to_string()))
    }
}

impl VfsFsError {
    /// Convert to WASI error code.
    pub fn downcast(self) -> anyhow::Result<generated::wasi::filesystem::types::ErrorCode> {
        Ok(crate::wasi_impl::vfs_error_to_error_code(&self.0))
    }
}

mod generated {
    // Re-export our types so bindgen can see them
    pub use super::{VfsDescriptor, VfsFsError, VfsReaddirIterator};

    wasmtime::component::bindgen!({
        path: "wit",
        world: "virtual-filesystem",
        trappable_error_type: {
            "wasi:filesystem/types.error-code" => VfsFsError,
        },
        with: {
            // Map to our custom VFS types (must be pub in this module)
            "wasi:filesystem/types.descriptor": VfsDescriptor,
            "wasi:filesystem/types.directory-entry-stream": VfsReaddirIterator,
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

/// Result type for VFS filesystem operations.
pub type VfsFsResult<T> = Result<T, VfsFsError>;

// Re-export wasmtime-wasi types that we need
pub use wasmtime_wasi::{DirPerms, FilePerms};

use wasmtime::component::HasData;

use crate::storage::VfsStorage;
use crate::wasi_impl::VfsState;

/// Marker type for implementing filesystem Host traits via VfsView.
///
/// This is similar to `WasiFilesystem` from wasmtime-wasi but uses our
/// VFS storage backend instead of the native filesystem.
pub struct VfsFilesystem<S: VfsStorage + 'static>(std::marker::PhantomData<S>);

impl<S: VfsStorage + 'static> HasData for VfsFilesystem<S> {
    type Data<'a> = VfsState<'a, S>;
}
