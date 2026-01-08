//! Sandbox wrapper for Python.
//!
//! Provides the main `Sandbox` class that Python users interact with.

use std::path::PathBuf;
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
    /// We use Arc<Runtime> to allow sharing with PreInitializedRuntime.
    runtime: Arc<tokio::runtime::Runtime>,
}

#[pymethods]
impl Sandbox {
    /// Create a new sandbox with the embedded Python runtime.
    ///
    /// Args:
    ///     site_packages: Optional path to a directory containing Python packages.
    ///         This directory will be mounted at `/site-packages` in the sandbox
    ///         and added to Python's import path.
    ///     packages: Optional list of paths to Python packages (.whl or .tar.gz files).
    ///         Packages are extracted and their contents added to the sandbox.
    ///         Packages with native extensions (.so files) are automatically
    ///         late-linked into the WebAssembly component.
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
    ///
    ///     # Sandbox with custom limits
    ///     limits = ResourceLimits(execution_timeout_ms=5000)
    ///     sandbox = Sandbox(resource_limits=limits)
    ///
    ///     # Sandbox with packages
    ///     sandbox = Sandbox(
    ///         packages=[
    ///             "/path/to/jinja2-3.1.2-py3-none-any.whl",
    ///             "/path/to/markupsafe-2.1.3-wasi.tar.gz",
    ///         ]
    ///     )
    ///
    ///     # Sandbox with pre-extracted site-packages
    ///     sandbox = Sandbox(site_packages="/path/to/site-packages")
    #[new]
    #[pyo3(signature = (*, site_packages=None, packages=None, resource_limits=None))]
    fn new(
        site_packages: Option<PathBuf>,
        packages: Option<Vec<PathBuf>>,
        resource_limits: Option<ResourceLimits>,
    ) -> PyResult<Self> {
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

        // Add site-packages directory if provided
        if let Some(path) = site_packages {
            builder = builder.with_site_packages(path);
        }

        // Add packages if provided
        if let Some(package_paths) = packages {
            for path in package_paths {
                builder = builder.with_package(&path).map_err(eryx_error_to_py)?;
            }
        }

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
    /// This is used internally by PreInitializedRuntime to create sandboxes.
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
