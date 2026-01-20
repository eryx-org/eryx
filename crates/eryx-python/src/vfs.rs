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
#[pyclass(module = "eryx")]
#[derive(Clone)]
pub struct VfsStorage {
    /// The underlying storage, used when Session bindings are added.
    #[allow(dead_code)]
    pub(crate) inner: Arc<eryx::vfs::InMemoryStorage>,
}

#[pymethods]
impl VfsStorage {
    /// Create a new in-memory filesystem storage.
    ///
    /// The storage starts empty. Files can be written by Python code running
    /// in a sandbox configured with this storage.
    ///
    /// Example:
    ///     storage = VfsStorage()
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(eryx::vfs::InMemoryStorage::new()),
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
