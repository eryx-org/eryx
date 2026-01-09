//! Sandbox wrapper for Python.
//!
//! Provides the main `Sandbox` class that Python users interact with.

use std::sync::Arc;

use pyo3::prelude::*;

use crate::error::{InitializationError, eryx_error_to_py};
use crate::resource_limits::ResourceLimits;
use crate::result::ExecuteResult;

/// A Python sandbox powered by WebAssembly.
///
/// The Sandbox executes Python code in complete isolation from the host system.
/// Each sandbox has its own memory space and cannot access files, network,
/// or other system resources unless explicitly provided via callbacks.
///
/// Example:
///     # Basic sandbox
///     sandbox = Sandbox()
///     result = sandbox.execute('print("Hello from the sandbox!")')
///     print(result.stdout)  # "Hello from the sandbox!"
///
///     # Sandbox with packages (e.g., jinja2)
///     sandbox = Sandbox(
///         packages=["/path/to/jinja2-3.1.2-py3-none-any.whl"],
///         site_packages="/path/to/extracted/site-packages",
///     )
///     result = sandbox.execute('from jinja2 import Template; print(Template("{{ x }}").render(x=42))')
#[pyclass(module = "eryx")]
pub struct Sandbox {
    // Note: We don't derive Debug because tokio::runtime::Runtime doesn't implement it.
    // The __repr__ method provides Python-side introspection instead.
    /// The underlying eryx Sandbox.
    inner: eryx::Sandbox,
    /// Tokio runtime for executing async code.
    /// We use Arc<Runtime> to allow sharing with SandboxFactory.
    runtime: Arc<tokio::runtime::Runtime>,
}

#[pymethods]
impl Sandbox {
    /// Create a new sandbox with the embedded Python runtime.
    ///
    /// This creates a fast sandbox (~1-5ms) using the pre-initialized Python runtime.
    /// The sandbox has access to Python's standard library but no third-party packages.
    ///
    /// For sandboxes with custom packages, use `SandboxFactory` instead.
    ///
    /// Args:
    ///     resource_limits: Optional resource limits for execution.
    ///
    /// Returns:
    ///     A new Sandbox instance ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If the sandbox fails to initialize.
    ///
    /// Example:
    ///     # Default sandbox (stdlib only)
    ///     sandbox = Sandbox()
    ///     result = sandbox.execute('import json; print(json.dumps([1, 2, 3]))')
    ///
    ///     # Sandbox with custom limits
    ///     limits = ResourceLimits(execution_timeout_ms=5000)
    ///     sandbox = Sandbox(resource_limits=limits)
    ///
    ///     # For custom packages, use SandboxFactory instead:
    ///     factory = SandboxFactory(
    ///         packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
    ///         imports=["jinja2"],
    ///     )
    ///     sandbox = factory.create_sandbox()
    #[new]
    #[pyo3(signature = (*, resource_limits=None))]
    fn new(resource_limits: Option<ResourceLimits>) -> PyResult<Self> {
        // Create a tokio runtime for async execution
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );

        // Build the eryx sandbox with embedded runtime
        let mut builder = eryx::Sandbox::embedded();

        // Apply resource limits if provided
        if let Some(limits) = resource_limits {
            builder = builder.with_resource_limits(limits.into());
        }

        let inner = builder.build().map_err(eryx_error_to_py)?;

        Ok(Self { inner, runtime })
    }

    /// Execute Python code in the sandbox.
    ///
    /// The code runs in complete isolation. Any output to stdout is captured
    /// and returned in the result.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///
    /// Returns:
    ///     ExecuteResult containing stdout, timing info, and statistics.
    ///
    /// Raises:
    ///     ExecutionError: If the Python code raises an exception.
    ///     TimeoutError: If execution exceeds the timeout limit.
    ///     ResourceLimitError: If a resource limit is exceeded.
    ///
    /// Example:
    ///     result = sandbox.execute('''
    ///     x = 2 + 2
    ///     print(f"2 + 2 = {x}")
    ///     ''')
    ///     print(result.stdout)  # "2 + 2 = 4"
    fn execute(&self, py: Python<'_>, code: &str) -> PyResult<ExecuteResult> {
        // Release the GIL while executing in the sandbox
        // This allows other Python threads to run during sandbox execution
        let code = code.to_string();
        let runtime = self.runtime.clone();
        py.allow_threads(|| {
            runtime
                .block_on(self.inner.execute(&code))
                .map(ExecuteResult::from)
                .map_err(eryx_error_to_py)
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "Sandbox(resource_limits={:?})",
            self.inner.resource_limits()
        )
    }
}

// Sandbox holds a tokio runtime which is Send + Sync
unsafe impl Send for Sandbox {}

impl Sandbox {
    /// Create a Sandbox from an existing eryx::Sandbox.
    ///
    /// This is used internally by SandboxFactory to create sandboxes.
    ///
    /// # Errors
    ///
    /// Returns an error if the tokio runtime cannot be created.
    pub(crate) fn from_inner(inner: eryx::Sandbox) -> PyResult<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );
        Ok(Self { inner, runtime })
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("resource_limits", self.inner.resource_limits())
            .finish_non_exhaustive()
    }
}
