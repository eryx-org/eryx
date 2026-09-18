//! Sandbox pool for Python.
//!
//! Provides `SandboxPool` and `PooledSandbox` for bounded concurrent
//! execution with automatic lease lifecycle management.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::callback::extract_callbacks;
use crate::error::pool_error_to_py;
use crate::resource_limits::ResourceLimits;
use crate::result::ExecuteResult;
use crate::sandbox::PyOutputHandler;

/// A managed pool of warm sandbox instances.
///
/// Provides bounded concurrent execution with automatic lease lifecycle.
/// Create via ``SandboxFactory.create_pool()``.
///
/// Example:
///     factory = SandboxFactory(imports=["json"], cache=True)
///     pool = factory.create_pool(max_size=4, min_idle=1)
///
///     with pool.acquire(resource_limits=ResourceLimits(execution_timeout_ms=1000)) as sandbox:
///         result = sandbox.execute('import json; print(json.dumps([1, 2]))')
///
///     pool.close()
#[pyclass(module = "eryx")]
pub struct SandboxPool {
    inner: eryx::SandboxPool,
    runtime: Arc<tokio::runtime::Runtime>,
    /// Factory's default callbacks, used when acquire() is called without callbacks.
    default_callbacks: Option<Py<PyAny>>,
    /// Keep extracted package temp dirs alive for the pool's lifetime.
    #[allow(dead_code)]
    _extracted_packages: Arc<Vec<eryx::ExtractedPackage>>,
    /// Keep factory data-files temp dir alive for the pool's lifetime.
    #[allow(dead_code)]
    _data_files_dir: Option<Arc<tempfile::TempDir>>,
}

/// Statistics about pool usage.
///
/// Returned by ``SandboxPool.stats()``.
#[pyclass(module = "eryx")]
#[derive(Debug)]
pub struct PoolStats {
    inner: eryx::PoolStats,
}

#[pymethods]
impl PoolStats {
    /// Total number of sandboxes tracked by the pool (in use + idle).
    #[getter]
    fn total(&self) -> usize {
        self.inner.total
    }

    /// Number of warm sandboxes sitting idle in the pool queue.
    #[getter]
    fn idle(&self) -> usize {
        self.inner.idle
    }

    /// Remaining semaphore permits (concurrency capacity, not idle sandbox count).
    #[getter]
    fn available(&self) -> usize {
        self.inner.available
    }

    /// Number of sandboxes currently in use.
    #[getter]
    fn in_use(&self) -> usize {
        self.inner.in_use
    }

    /// Total successful acquisitions since pool creation.
    #[getter]
    fn total_acquisitions(&self) -> u64 {
        self.inner.total_acquisitions
    }

    /// Total sandbox creations (initial + on-demand).
    #[getter]
    fn total_creations(&self) -> u64 {
        self.inner.total_creations
    }

    /// Number of acquisitions that had to wait for a sandbox.
    #[getter]
    fn wait_count(&self) -> u64 {
        self.inner.wait_count
    }

    /// Cumulative wait time in milliseconds.
    #[getter]
    fn total_wait_time_ms(&self) -> f64 {
        self.inner.total_wait_time.as_secs_f64() * 1000.0
    }

    /// Average wait time in milliseconds (0.0 if no waits).
    #[getter]
    fn average_wait_time_ms(&self) -> f64 {
        self.inner.average_wait_time().as_secs_f64() * 1000.0
    }

    fn __repr__(&self) -> String {
        format!(
            "PoolStats(total={}, idle={}, in_use={}, acquisitions={})",
            self.inner.total, self.inner.idle, self.inner.in_use, self.inner.total_acquisitions,
        )
    }
}

/// A sandbox lease acquired from a pool.
///
/// Returns to the pool automatically when the context manager exits or
/// ``release()`` is called. Use-after-release raises ``ValueError``.
///
/// Example:
///     with pool.acquire() as sandbox:
///         result = sandbox.execute('print("Hello!")')
///     # sandbox is returned to the pool here
#[pyclass(module = "eryx")]
pub struct PooledSandbox {
    inner: Option<eryx::PooledSandbox>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for PooledSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledSandbox")
            .field("active", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

#[pymethods]
impl PooledSandbox {
    /// Execute Python code in the sandboxed lease.
    ///
    /// Raises:
    ///     ValueError: If the sandbox has already been released.
    ///     ExecutionError: If the Python code raises an exception.
    ///     TimeoutError: If execution exceeds the timeout limit.
    fn execute(&self, py: Python<'_>, code: &str) -> PyResult<ExecuteResult> {
        let sandbox = self
            .inner
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("sandbox has been released back to pool"))?;

        let code = code.to_string();
        let runtime = self.runtime.clone();
        py.detach(|| {
            runtime
                .block_on(sandbox.execute(&code))
                .map(ExecuteResult::from)
                .map_err(crate::error::eryx_error_to_py)
        })
    }

    /// Release the sandbox back to the pool.
    ///
    /// Idempotent — calling release() on an already-released sandbox is a no-op.
    fn release(&mut self) {
        self.inner.take();
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &mut self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        self.release();
        false
    }

    fn __repr__(&self) -> String {
        if self.inner.is_some() {
            "PooledSandbox(active)".to_string()
        } else {
            "PooledSandbox(released)".to_string()
        }
    }
}

#[pymethods]
impl SandboxPool {
    /// Acquire a sandbox from the pool.
    ///
    /// Blocks until a sandbox is available or the acquire timeout is reached.
    /// The GIL is released while waiting so other Python threads can make
    /// progress (including releasing their own leases).
    ///
    /// Args:
    ///     resource_limits: Optional per-request resource limits.
    ///     callbacks: Optional per-request callbacks.
    ///     on_stdout: Optional per-request stdout streaming callback.
    ///     on_stderr: Optional per-request stderr streaming callback.
    ///
    /// Returns:
    ///     A ``PooledSandbox`` that should be used as a context manager.
    ///
    /// Raises:
    ///     PoolClosedError: If the pool has been closed.
    ///     PoolTimeoutError: If the acquire timeout is reached.
    #[pyo3(signature = (*, resource_limits=None, callbacks=None, on_stdout=None, on_stderr=None))]
    fn acquire(
        &self,
        py: Python<'_>,
        resource_limits: Option<ResourceLimits>,
        callbacks: Option<Bound<'_, PyAny>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
    ) -> PyResult<PooledSandbox> {
        let runtime = self.runtime.clone();

        // Release GIL while waiting for a sandbox
        let mut pooled: eryx::PooledSandbox = py.detach(|| {
            runtime
                .block_on(self.inner.acquire())
                .map_err(pool_error_to_py)
        })?;

        // Set per-request state while we hold the GIL
        self.apply_per_request_state(
            py,
            &mut pooled,
            resource_limits,
            callbacks,
            on_stdout,
            on_stderr,
        )?;

        Ok(PooledSandbox {
            inner: Some(pooled),
            runtime: self.runtime.clone(),
        })
    }

    /// Try to acquire a sandbox without blocking.
    ///
    /// Returns ``None`` if no sandbox is immediately available.
    ///
    /// Args:
    ///     resource_limits: Optional per-request resource limits.
    ///     callbacks: Optional per-request callbacks.
    ///     on_stdout: Optional per-request stdout streaming callback.
    ///     on_stderr: Optional per-request stderr streaming callback.
    ///
    /// Raises:
    ///     PoolClosedError: If the pool has been closed.
    #[pyo3(signature = (*, resource_limits=None, callbacks=None, on_stdout=None, on_stderr=None))]
    fn try_acquire(
        &self,
        py: Python<'_>,
        resource_limits: Option<ResourceLimits>,
        callbacks: Option<Bound<'_, PyAny>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
    ) -> PyResult<Option<PooledSandbox>> {
        let maybe_pooled = self.inner.try_acquire().map_err(pool_error_to_py)?;

        match maybe_pooled {
            Some(mut pooled) => {
                self.apply_per_request_state(
                    py,
                    &mut pooled,
                    resource_limits,
                    callbacks,
                    on_stdout,
                    on_stderr,
                )?;
                Ok(Some(PooledSandbox {
                    inner: Some(pooled),
                    runtime: self.runtime.clone(),
                }))
            }
            None => Ok(None),
        }
    }

    /// Get current pool statistics.
    fn stats(&self) -> PoolStats {
        PoolStats {
            inner: self.inner.stats(),
        }
    }

    /// Evict idle sandboxes that have exceeded the idle timeout.
    ///
    /// Maintains at least ``min_idle`` instances. Returns the number evicted.
    fn evict_idle(&self) -> usize {
        self.inner.evict_idle()
    }

    /// Close the pool, preventing new acquisitions.
    ///
    /// Blocked ``acquire()`` calls are woken with ``PoolClosedError``.
    /// Idle sandboxes are dropped. Existing leases continue to work
    /// but are not returned to the pool on release.
    fn close(&self) {
        self.inner.close();
    }

    /// Whether the pool has been closed.
    #[getter]
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        let stats = self.inner.stats();
        format!(
            "SandboxPool(total={}, idle={}, in_use={}, closed={})",
            stats.total,
            stats.idle,
            stats.in_use,
            self.inner.is_closed(),
        )
    }
}

impl SandboxPool {
    /// Construct from Rust internals (called by `SandboxFactory.create_pool`).
    pub(crate) fn from_inner(
        inner: eryx::SandboxPool,
        runtime: Arc<tokio::runtime::Runtime>,
        default_callbacks: Option<Py<PyAny>>,
        extracted_packages: Arc<Vec<eryx::ExtractedPackage>>,
        data_files_dir: Option<Arc<tempfile::TempDir>>,
    ) -> Self {
        Self {
            inner,
            runtime,
            default_callbacks,
            _extracted_packages: extracted_packages,
            _data_files_dir: data_files_dir,
        }
    }

    /// Apply per-request callbacks, resource limits, and output handlers
    /// to a freshly acquired `PooledSandbox`.
    fn apply_per_request_state(
        &self,
        py: Python<'_>,
        pooled: &mut eryx::PooledSandbox,
        resource_limits: Option<ResourceLimits>,
        callbacks: Option<Bound<'_, PyAny>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        // Resolve callbacks: caller's, or fall back to factory defaults.
        let resolved_callbacks = callbacks.or_else(|| {
            self.default_callbacks
                .as_ref()
                .map(|cbs| cbs.bind(py).clone())
        });

        if let Some(ref cbs) = resolved_callbacks {
            let rust_callbacks = extract_callbacks(py, cbs)?;
            let boxed: Vec<Box<dyn eryx::Callback>> = rust_callbacks
                .into_iter()
                .map(|cb| Box::new(cb) as Box<dyn eryx::Callback>)
                .collect();
            pooled.sandbox_mut().set_callbacks(boxed);
        }

        if let Some(limits) = resource_limits {
            pooled.sandbox_mut().set_resource_limits((&limits).into());
        }

        if on_stdout.is_some() || on_stderr.is_some() {
            pooled.sandbox_mut().set_output_handler(PyOutputHandler {
                on_stdout,
                on_stderr,
            });
        }

        Ok(())
    }
}

impl std::fmt::Debug for SandboxPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxPool")
            .field("closed", &self.inner.is_closed())
            .finish_non_exhaustive()
    }
}
