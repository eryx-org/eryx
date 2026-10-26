//! ResourceLimits wrapper for Python.
//!
//! Exposes sandbox resource limit configuration to Python.

use pyo3::prelude::*;
use std::time::Duration;

/// Resource limits for sandbox execution.
///
/// Use this class to configure execution timeouts, memory limits,
/// and callback restrictions for a sandbox.
///
/// Example:
///     limits = ResourceLimits(
///         execution_timeout_ms=5000,  # 5 second timeout
///         max_memory_bytes=100_000_000,  # 100MB memory limit
///     )
///     sandbox = Sandbox(resource_limits=limits)
#[pyclass(module = "eryx", from_py_object)]
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum execution time in milliseconds.
    #[pyo3(get, set)]
    pub execution_timeout_ms: Option<u64>,

    /// Maximum time for a single callback invocation in milliseconds.
    #[pyo3(get, set)]
    pub callback_timeout_ms: Option<u64>,

    /// Maximum memory usage in bytes.
    #[pyo3(get, set)]
    pub max_memory_bytes: Option<u64>,

    /// Maximum number of callback invocations.
    #[pyo3(get, set)]
    pub max_callback_invocations: Option<u32>,

    /// Maximum fuel (instructions) allowed.
    #[pyo3(get, set)]
    pub max_fuel: Option<u64>,

    /// Maximum total bytes the in-memory virtual filesystem may hold.
    ///
    /// VFS files live in host memory, outside `max_memory_bytes`, and the script
    /// picks its own write offsets. `None` uses the default (64 MiB).
    #[pyo3(get, set)]
    pub max_vfs_bytes: Option<u64>,

    /// Maximum host files (stdlib, site-packages and volume files) a script
    /// may hold open at once. Opens past it fail with `EDQUOT`. `None` uses
    /// the default (256).
    #[pyo3(get, set)]
    pub max_open_host_files: Option<usize>,
}

#[pymethods]
impl ResourceLimits {
    /// Create new resource limits.
    ///
    /// All parameters are optional. If not specified, defaults are used:
    /// - execution_timeout_ms: 30000 (30 seconds)
    /// - callback_timeout_ms: 10000 (10 seconds)
    /// - max_memory_bytes: 134217728 (128 MB)
    /// - max_callback_invocations: 1000
    /// - max_fuel: None (unlimited)
    /// - max_vfs_bytes: None (64 MB)
    /// - max_open_host_files: None (256)
    ///
    /// Set a parameter to `None` explicitly to disable that specific limit.
    /// `max_vfs_bytes` and `max_open_host_files` are the exceptions: `None`
    /// means the default cap, since both bound host resources.
    #[new]
    #[pyo3(signature = (*, execution_timeout_ms=30000, callback_timeout_ms=10000, max_memory_bytes=134217728, max_callback_invocations=1000, max_fuel=None, max_vfs_bytes=None, max_open_host_files=None))]
    fn new(
        execution_timeout_ms: Option<u64>,
        callback_timeout_ms: Option<u64>,
        max_memory_bytes: Option<u64>,
        max_callback_invocations: Option<u32>,
        max_fuel: Option<u64>,
        max_vfs_bytes: Option<u64>,
        max_open_host_files: Option<usize>,
    ) -> Self {
        Self {
            execution_timeout_ms,
            callback_timeout_ms,
            max_memory_bytes,
            max_callback_invocations,
            max_fuel,
            max_vfs_bytes,
            max_open_host_files,
        }
    }

    /// Create resource limits with no restrictions.
    ///
    /// Warning: Use with caution! Code can run indefinitely and use unlimited memory.
    ///
    /// The virtual filesystem cap still applies (see `max_vfs_bytes`): it bounds
    /// *host* memory, so removing it would let a script kill the process. Pass a
    /// large `max_vfs_bytes` explicitly if you need more than the default.
    #[staticmethod]
    fn unlimited() -> Self {
        Self {
            execution_timeout_ms: None,
            callback_timeout_ms: None,
            max_memory_bytes: None,
            max_callback_invocations: None,
            max_fuel: None,
            max_vfs_bytes: None,
            max_open_host_files: None,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "ResourceLimits(execution_timeout_ms={:?}, callback_timeout_ms={:?}, max_memory_bytes={:?}, max_callback_invocations={:?}, max_fuel={:?}, max_vfs_bytes={:?}, max_open_host_files={:?})",
            self.execution_timeout_ms,
            self.callback_timeout_ms,
            self.max_memory_bytes,
            self.max_callback_invocations,
            self.max_fuel,
            self.max_vfs_bytes,
            self.max_open_host_files,
        )
    }
}

/// The same defaults as `ResourceLimits()` in Python.
impl Default for ResourceLimits {
    fn default() -> Self {
        Self::new(
            Some(30_000),
            Some(10_000),
            Some(134_217_728),
            Some(1000),
            None,
            None,
            None,
        )
    }
}

impl From<&ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: &ResourceLimits) -> Self {
        // Every field is set explicitly, so the starting point does not matter;
        // `eryx::ResourceLimits` is #[non_exhaustive], hence the builder.
        eryx::ResourceLimits::default()
            .with_execution_timeout(limits.execution_timeout_ms.map(Duration::from_millis))
            .with_callback_timeout(limits.callback_timeout_ms.map(Duration::from_millis))
            .with_max_memory_bytes(limits.max_memory_bytes)
            .with_max_callback_invocations(limits.max_callback_invocations)
            .with_max_fuel(limits.max_fuel)
            .with_max_vfs_bytes(limits.max_vfs_bytes)
            .with_max_open_host_files(limits.max_open_host_files)
    }
}

impl From<ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: ResourceLimits) -> Self {
        (&limits).into()
    }
}
