//! Error types and conversion for Python bindings.
//!
//! Maps Eryx errors to Python exceptions with appropriate types.

use pyo3::exceptions::{PyException, PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::{PyErr, create_exception};

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
create_exception!(
    eryx,
    SandboxTimeoutError,
    PyTimeoutError,
    "Execution timed out."
);

/// Convert an `eryx::Error` to a Python exception.
///
/// This is a free function rather than a `From` impl due to the orphan rule:
/// we can't implement `From<eryx::Error> for PyErr` since neither type is
/// defined in this crate.
pub fn eryx_error_to_py(err: eryx::Error) -> PyErr {
    match err {
        eryx::Error::Initialization(msg) => InitializationError::new_err(msg),
        eryx::Error::WasmEngine(msg) => InitializationError::new_err(msg),
        eryx::Error::WasmComponent(e) => InitializationError::new_err(e.to_string()),
        eryx::Error::Execution(msg) => {
            // Check if this is a timeout error wrapped in an Execution error
            // The eryx crate converts timeouts to string messages like "Execution timed out after Xms"
            if msg.contains("timed out") {
                SandboxTimeoutError::new_err(msg)
            } else {
                ExecutionError::new_err(msg)
            }
        }
        eryx::Error::Callback(e) => ExecutionError::new_err(e.to_string()),
        eryx::Error::ResourceLimit(msg) => ResourceLimitError::new_err(msg),
        eryx::Error::Timeout(duration) => {
            SandboxTimeoutError::new_err(format!("execution timed out after {duration:?}"))
        }
        eryx::Error::Serialization(msg) => PyValueError::new_err(msg),
        eryx::Error::MissingPythonStdlib => InitializationError::new_err(
            "Python stdlib not found. Use Sandbox() which includes embedded runtime.",
        ),
        eryx::Error::Snapshot(msg) => InitializationError::new_err(msg),
        eryx::Error::Io(e) => PyRuntimeError::new_err(e.to_string()),
    }
}

/// Register exception types with the Python module.
pub fn register_exceptions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("EryxError", m.py().get_type::<EryxError>())?;
    m.add("ExecutionError", m.py().get_type::<ExecutionError>())?;
    m.add(
        "InitializationError",
        m.py().get_type::<InitializationError>(),
    )?;
    m.add(
        "ResourceLimitError",
        m.py().get_type::<ResourceLimitError>(),
    )?;
    m.add("TimeoutError", m.py().get_type::<SandboxTimeoutError>())?;
    Ok(())
}
