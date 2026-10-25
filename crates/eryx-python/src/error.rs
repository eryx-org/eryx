//! Error types and conversion for Python bindings.
//!
//! Maps Eryx errors to Python exceptions with appropriate types.

use std::sync::{Arc, OnceLock};

use pyo3::exceptions::{PyException, PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::{PyErr, create_exception, import_exception};

// Define custom exception types that mirror the Rust error hierarchy
create_exception!(
    eryx,
    EryxError,
    PyException,
    "Base exception for all Eryx errors."
);
create_exception!(
    eryx,
    ExecutionError,
    EryxError,
    "Error during Python code execution in the sandbox."
);
create_exception!(
    eryx,
    PythonExceptionError,
    ExecutionError,
    "The sandboxed Python code raised an uncaught exception."
);
create_exception!(
    eryx,
    InitializationError,
    EryxError,
    "Error during sandbox initialization."
);
create_exception!(
    eryx,
    ResourceLimitError,
    EryxError,
    "Resource limit exceeded during execution."
);
// `TimeoutError` derives from both the built-in `TimeoutError` and
// `EryxError`. `create_exception!` only supports a single base, so the class is
// built in `register_exceptions` and looked up from the module when raised.
import_exception!(eryx._eryx, TimeoutError);
create_exception!(eryx, CancelledError, EryxError, "Execution was cancelled.");
// Raised by user callbacks (not by eryx), so it derives from Exception rather
// than EryxError.
create_exception!(
    eryx,
    SuspendCallback,
    PyException,
    "Raise from a callback to suspend execution; the argument is an opaque reason string."
);

// Pool-specific exceptions
create_exception!(eryx, PoolError, EryxError, "Base pool error.");
create_exception!(
    eryx,
    PoolExhaustedError,
    PoolError,
    "All sandboxes in the pool are in use."
);
create_exception!(
    eryx,
    PoolTimeoutError,
    PoolError,
    "Timed out waiting for an available sandbox."
);
create_exception!(eryx, PoolClosedError, PoolError, "Pool has been closed.");

/// Convert an `eryx::Error` to a Python exception.
///
/// This is a free function rather than a `From` impl due to the orphan rule:
/// we can't implement `From<eryx::Error> for PyErr` since neither type is
/// defined in this crate.
pub fn eryx_error_to_py(err: eryx::Error) -> PyErr {
    match err {
        eryx::Error::Initialization(msg) => InitializationError::new_err(msg),
        eryx::Error::WasmEngine(msg) => InitializationError::new_err(msg),
        eryx::Error::EpochTicker(msg) => ExecutionError::new_err(msg),
        eryx::Error::WasmComponent(e) => InitializationError::new_err(e.to_string()),
        eryx::Error::Execution(msg) => ExecutionError::new_err(msg),
        // A subclass of ExecutionError, so callers can tell a script error from
        // a sandbox failure while `except ExecutionError` still catches both.
        eryx::Error::PythonException(msg) => PythonExceptionError::new_err(msg),
        eryx::Error::Callback(e) => ExecutionError::new_err(e.to_string()),
        eryx::Error::ResourceLimit(msg) => ResourceLimitError::new_err(msg),
        eryx::Error::Timeout(duration) => {
            TimeoutError::new_err(format!("execution timed out after {duration:?}"))
        }
        eryx::Error::Serialization(msg) => PyValueError::new_err(msg),
        eryx::Error::MissingPythonStdlib => InitializationError::new_err(
            "Python stdlib not found. Use Sandbox() which includes embedded runtime.",
        ),
        eryx::Error::Snapshot(msg) => InitializationError::new_err(msg),
        eryx::Error::Cancelled => CancelledError::new_err("execution was cancelled"),
        eryx::Error::Suspended(reason) => {
            ExecutionError::new_err(format!("execution suspended: {reason}"))
        }
        eryx::Error::FuelExhausted { consumed, limit } => ResourceLimitError::new_err(format!(
            "execution ran out of fuel after {consumed} instructions (limit: {limit})"
        )),
        eryx::Error::Io(e) => PyRuntimeError::new_err(e.to_string()),
        // `eryx::Error` is `#[non_exhaustive]`; map any future variants to a
        // generic Eryx error rather than failing to compile.
        other => EryxError::new_err(other.to_string()),
    }
}

/// Get the process-wide Tokio runtime shared by every sandbox, session,
/// factory and MCP manager, creating it on first use.
pub fn make_runtime() -> PyResult<Arc<tokio::runtime::Runtime>> {
    static RUNTIME: OnceLock<Arc<tokio::runtime::Runtime>> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(Arc::clone(runtime));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| InitializationError::new_err(format!("failed to create runtime: {e}")))?;
    // A racing thread may have won; its runtime is kept and ours is dropped.
    Ok(Arc::clone(RUNTIME.get_or_init(|| Arc::new(runtime))))
}

/// Convert an `eryx::PoolError` to a Python exception.
pub fn pool_error_to_py(err: eryx::PoolError) -> PyErr {
    match err {
        eryx::PoolError::Exhausted(n) => {
            PoolExhaustedError::new_err(format!("all {n} sandboxes are in use"))
        }
        eryx::PoolError::Timeout(d) => {
            PoolTimeoutError::new_err(format!("acquire timed out after {d:?}"))
        }
        eryx::PoolError::Closed => PoolClosedError::new_err("pool is closed"),
        eryx::PoolError::Creation(msg) => InitializationError::new_err(msg),
        eryx::PoolError::Reset(msg) => PoolError::new_err(msg),
        other => PoolError::new_err(other.to_string()),
    }
}

/// Register exception types with the Python module.
pub fn register_exceptions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("EryxError", m.py().get_type::<EryxError>())?;
    m.add("ExecutionError", m.py().get_type::<ExecutionError>())?;
    m.add(
        "PythonExceptionError",
        m.py().get_type::<PythonExceptionError>(),
    )?;
    m.add(
        "InitializationError",
        m.py().get_type::<InitializationError>(),
    )?;
    m.add(
        "ResourceLimitError",
        m.py().get_type::<ResourceLimitError>(),
    )?;
    let py = m.py();
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", "eryx")?;
    namespace.set_item("__doc__", "Execution timed out.")?;
    let timeout_error = py.get_type::<pyo3::types::PyType>().call1((
        "TimeoutError",
        (py.get_type::<PyTimeoutError>(), py.get_type::<EryxError>()),
        namespace,
    ))?;
    m.add("TimeoutError", timeout_error)?;
    m.add("CancelledError", m.py().get_type::<CancelledError>())?;
    m.add("SuspendCallback", m.py().get_type::<SuspendCallback>())?;
    m.add("PoolError", m.py().get_type::<PoolError>())?;
    m.add(
        "PoolExhaustedError",
        m.py().get_type::<PoolExhaustedError>(),
    )?;
    m.add("PoolTimeoutError", m.py().get_type::<PoolTimeoutError>())?;
    m.add("PoolClosedError", m.py().get_type::<PoolClosedError>())?;
    Ok(())
}
