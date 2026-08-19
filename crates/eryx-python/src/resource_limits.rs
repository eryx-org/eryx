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
    ///
    /// Set a parameter to `None` explicitly to disable that specific limit.
    /// `max_vfs_bytes` is the exception: `None` means the default cap, since an
    /// uncapped in-memory filesystem lets a script exhaust host memory.
    #[new]
    #[pyo3(signature = (*, execution_timeout_ms=30000, callback_timeout_ms=10000, max_memory_bytes=134217728, max_callback_invocations=1000, max_fuel=None, max_vfs_bytes=None))]
    fn new(
        execution_timeout_ms: Option<u64>,
        callback_timeout_ms: Option<u64>,
        max_memory_bytes: Option<u64>,
        max_callback_invocations: Option<u32>,
        max_fuel: Option<u64>,
        max_vfs_bytes: Option<u64>,
    ) -> Self {
        Self {
            execution_timeout_ms,
            callback_timeout_ms,
            max_memory_bytes,
            max_callback_invocations,
            max_fuel,
            max_vfs_bytes,
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
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "ResourceLimits(execution_timeout_ms={:?}, callback_timeout_ms={:?}, max_memory_bytes={:?}, max_callback_invocations={:?}, max_fuel={:?}, max_vfs_bytes={:?})",
            self.execution_timeout_ms,
            self.callback_timeout_ms,
            self.max_memory_bytes,
            self.max_callback_invocations,
            self.max_fuel,
            self.max_vfs_bytes,
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
    }
}

impl From<ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: ResourceLimits) -> Self {
        (&limits).into()
    }
}
