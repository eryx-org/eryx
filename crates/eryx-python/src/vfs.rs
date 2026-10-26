//! Virtual filesystem configuration for Python bindings.
//!
//! Provides a Python-accessible wrapper around the VFS storage.

use std::sync::Arc;

use pyo3::prelude::*;

/// An in-memory filesystem storage for sandboxed Python code.
///
/// VfsStorage provides a sandboxed filesystem that Python code can use to
/// read and write files without accessing the host filesystem. The storage
/// persists across multiple executions within a Session.
///
/// Note: VfsStorage is only available with Sessions, not with regular Sandbox.
/// Regular Sandbox executions are fully isolated with no persistent state.
///
/// Example:
///     # Create storage and use it with a Session
///     storage = VfsStorage()
///     session = Session(vfs=storage)
///
///     # Write a file
///     session.execute('open("/data/test.txt", "w").write("hello")')
///
///     # Storage persists across executions within the session
///     result = session.execute('print(open("/data/test.txt").read())')
///     print(result.stdout)  # "hello"
#[pyclass(module = "eryx", from_py_object)]
#[derive(Clone)]
pub struct VfsStorage {
    /// The underlying type-erased storage.
    pub(crate) inner: eryx::vfs::ArcStorage,
}

impl VfsStorage {
    /// Convert into the underlying `ArcStorage`.
    pub(crate) fn into_arc_storage(self) -> eryx::vfs::ArcStorage {
        self.inner
    }
}

#[pymethods]
impl VfsStorage {
    /// Create a new in-memory filesystem storage.
    ///
    /// The storage starts empty. Files can be written by Python code running
    /// in a sandbox configured with this storage.
    ///
    /// Args:
    ///     max_bytes: Cap on the total size of all file contents combined,
    ///         plus a charge of the path length and 64 bytes for each file and
    ///         directory. Writes that would exceed it fail inside the sandbox with
    ///         `OSError`. `None` (the default) means 64 MiB. The storage owns this
    ///         cap, so `ResourceLimits.max_vfs_bytes` does not apply to it.
    ///
    /// Example:
    ///     storage = VfsStorage(max_bytes=1024 * 1024)
    #[new]
    #[pyo3(signature = (*, max_bytes=None))]
    fn new(max_bytes: Option<u64>) -> Self {
        Self {
            inner: eryx::vfs::ArcStorage::new(Arc::new(eryx::vfs::ScrubbingStorage::new(
                eryx::vfs::InMemoryStorage::with_max_bytes(
                    max_bytes.unwrap_or(eryx::vfs::DEFAULT_MAX_BYTES),
                ),
                std::collections::HashMap::new(),
                eryx::vfs::VfsFileScrubPolicy::None,
            ))),
        }
    }

    fn __repr__(&self) -> String {
        "VfsStorage()".to_string()
    }
}

impl std::fmt::Debug for VfsStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsStorage").finish_non_exhaustive()
    }
}
