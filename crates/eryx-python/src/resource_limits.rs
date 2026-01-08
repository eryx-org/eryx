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
#[pyclass(module = "eryx")]
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
    ///
    /// Set a parameter to `None` explicitly to disable that specific limit.
    #[new]
    #[pyo3(signature = (*, execution_timeout_ms=30000, callback_timeout_ms=10000, max_memory_bytes=134217728, max_callback_invocations=1000))]
    fn new(
        execution_timeout_ms: Option<u64>,
        callback_timeout_ms: Option<u64>,
        max_memory_bytes: Option<u64>,
        max_callback_invocations: Option<u32>,
    ) -> Self {
        Self {
            execution_timeout_ms,
            callback_timeout_ms,
            max_memory_bytes,
            max_callback_invocations,
        }
    }

    /// Create resource limits with no restrictions.
    ///
    /// Warning: Use with caution! Code can run indefinitely and use unlimited memory.
    #[staticmethod]
    fn unlimited() -> Self {
        Self {
            execution_timeout_ms: None,
            callback_timeout_ms: None,
            max_memory_bytes: None,
            max_callback_invocations: None,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "ResourceLimits(execution_timeout_ms={:?}, callback_timeout_ms={:?}, max_memory_bytes={:?}, max_callback_invocations={:?})",
            self.execution_timeout_ms,
            self.callback_timeout_ms,
            self.max_memory_bytes,
            self.max_callback_invocations,
        )
    }
}

impl From<&ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: &ResourceLimits) -> Self {
        Self {
            execution_timeout: limits.execution_timeout_ms.map(Duration::from_millis),
            callback_timeout: limits.callback_timeout_ms.map(Duration::from_millis),
            max_memory_bytes: limits.max_memory_bytes,
            max_callback_invocations: limits.max_callback_invocations,
        }
    }
}

impl From<ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: ResourceLimits) -> Self {
        (&limits).into()
    }
}
