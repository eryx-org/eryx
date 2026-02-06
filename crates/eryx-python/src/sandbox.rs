//! Sandbox wrapper for Python.
//!
//! Provides the main `Sandbox` class that Python users interact with.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::net_config::NetConfig;
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
    /// Each call to `execute()` runs in complete isolation - Python state is not
    /// preserved between calls. For persistent state (including file storage),
    /// use `Session` instead.
    ///
    /// For sandboxes with custom packages, use `SandboxFactory` instead.
    ///
    /// Args:
    ///     resource_limits: Optional resource limits for execution.
    ///     network: Optional network configuration. If provided, enables networking.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
    ///         Can be a CallbackRegistry or a list of callback dicts.
    ///
    /// Returns:
    ///     A new Sandbox instance ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If the sandbox fails to initialize.
    ///
    /// Example:
    ///     # Default sandbox (stdlib only, no network)
    ///     sandbox = Sandbox()
    ///     result = sandbox.execute('import json; print(json.dumps([1, 2, 3]))')
    ///
    ///     # Sandbox with custom limits
    ///     limits = ResourceLimits(execution_timeout_ms=5000)
    ///     sandbox = Sandbox(resource_limits=limits)
    ///
    ///     # Sandbox with network access
    ///     net = NetConfig(allowed_hosts=["api.example.com"])
    ///     sandbox = Sandbox(network=net)
    ///
    ///     # Sandbox with callbacks
    ///     def get_time():
    ///         import time
    ///         return {"timestamp": time.time()}
    ///
    ///     sandbox = Sandbox(callbacks=[
    ///         {"name": "get_time", "fn": get_time, "description": "Returns current time"}
    ///     ])
    ///     result = sandbox.execute('t = await get_time(); print(t)')
    ///
    ///     # Sandbox with secrets
    ///     sandbox = Sandbox(
    ///         secrets={
    ///             "API_KEY": {"value": "sk-real-key", "allowed_hosts": ["api.example.com"]},
    ///         },
    ///         network=NetConfig(allowed_hosts=["api.example.com"]),
    ///     )
    ///
    ///     # For custom packages, use SandboxFactory instead:
    ///     factory = SandboxFactory(
    ///         packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
    ///         imports=["jinja2"],
    ///     )
    ///     sandbox = factory.create_sandbox()
    #[new]
    #[pyo3(signature = (*, resource_limits=None, network=None, callbacks=None, secrets=None, scrub_stdout=None, scrub_stderr=None, scrub_files=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        secrets: Option<Bound<'_, PyDict>>,
        scrub_stdout: Option<bool>,
        scrub_stderr: Option<bool>,
        scrub_files: Option<bool>,
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

        // Apply resource limits if provided
        if let Some(limits) = resource_limits {
            builder = builder.with_resource_limits(limits.into());
        }

        // Apply network config if provided
        if let Some(net) = network {
            builder = builder.with_network(net.into());
        }

        // Apply callbacks if provided
        if let Some(ref cbs) = callbacks {
            let python_callbacks = extract_callbacks(py, cbs)?;
            for callback in python_callbacks {
                builder = builder.with_callback(callback);
            }
        }

        // Apply secrets if provided
        let has_secrets = secrets.as_ref().is_some_and(|s| !s.is_empty());
        if let Some(ref secrets_dict) = secrets {
            builder = apply_secrets(builder, secrets_dict)?;
        }

        // Apply scrub policies (default to true when secrets are present)
        if scrub_stdout.unwrap_or(has_secrets) {
            builder = builder.scrub_stdout(true);
        }
        if scrub_stderr.unwrap_or(has_secrets) {
            builder = builder.scrub_stderr(true);
        }
        if scrub_files.unwrap_or(has_secrets) {
            builder = builder.scrub_files(true);
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
        let inner = &self.inner;
        py.detach(|| {
            runtime
                .block_on(inner.execute(&code))
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

// Static assertions that Sandbox is Send + Sync (required for PyO3 thread safety)
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
};

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

/// Parse a Python dict of secrets and apply them to the sandbox builder.
///
/// Expected format: `{"NAME": {"value": "secret", "allowed_hosts": ["host.com"]}}`
/// The `allowed_hosts` key is optional and defaults to an empty list.
pub(crate) fn apply_secrets<R, S>(
    mut builder: eryx::SandboxBuilder<R, S>,
    secrets_dict: &Bound<'_, PyDict>,
) -> PyResult<eryx::SandboxBuilder<R, S>> {
    for (key, value) in secrets_dict.iter() {
        let name: String = key
            .extract()
            .map_err(|_| pyo3::exceptions::PyTypeError::new_err("secret names must be strings"))?;

        let value_dict: Bound<'_, PyDict> = value.cast_into().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err(format!(
                "secret '{name}' value must be a dict with a 'value' key"
            ))
        })?;

        let secret_value: String = value_dict
            .get_item("value")?
            .ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "secret '{name}' dict must contain a 'value' key"
                ))
            })?
            .extract()
            .map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(format!(
                    "secret '{name}' value must be a string"
                ))
            })?;

        let allowed_hosts: Vec<String> = value_dict
            .get_item("allowed_hosts")?
            .map(|v| v.extract())
            .transpose()
            .map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(format!(
                    "secret '{name}' allowed_hosts must be a list of strings"
                ))
            })?
            .unwrap_or_default();

        builder = builder.with_secret(name, secret_value, allowed_hosts);
    }
    Ok(builder)
}
