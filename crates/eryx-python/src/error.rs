//! Error types and conversion for Python bindings.
//!
//! Maps Eryx errors to Python exceptions with appropriate types.

use std::sync::{Arc, Mutex};

use pyo3::exceptions::{PyException, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3::{PyErr, create_exception, import_exception};

// The exceptions eryx.run's client raises too, from the pure-Python `eryx-run`
// package, so `except eryx.TimeoutError` catches either's and code moves
// between local and hosted sandboxes unchanged.
import_exception!(eryx_run, EryxError);
import_exception!(eryx_run, ExecutionError);
import_exception!(eryx_run, PythonExceptionError);
import_exception!(eryx_run, InitializationError);
import_exception!(eryx_run, ResourceLimitError);
import_exception!(eryx_run, TimeoutError);
import_exception!(eryx_run, CancelledError);
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
///
/// What the run used before it failed is set on the exception as
/// `duration_ms`, `callback_invocations`, `peak_memory_bytes` and
/// `fuel_consumed`, like on `ExecuteResult`. All four are `None` if the failure
/// happened before the guest ran.
pub fn eryx_error_to_py(err: eryx::Error) -> PyErr {
    let stats = err.stats().cloned();
    let py_err = kind_to_py(err.into_kind());
    Python::attach(|py| set_stats(py, &py_err, stats.as_ref()));
    py_err
}

/// Attach the statistics of a failed run to its exception.
fn set_stats(py: Python<'_>, err: &PyErr, stats: Option<&eryx::ExecuteStats>) {
    let value = err.value(py);
    // Best effort: every exception raised here accepts attributes, and a
    // failure to set one must not replace the error being reported.
    let _ = value.setattr(
        "duration_ms",
        stats.map(|s| s.duration.as_secs_f64() * 1000.0),
    );
    let _ = value.setattr(
        "callback_invocations",
        stats.map(|s| s.callback_invocations),
    );
    let _ = value.setattr("peak_memory_bytes", stats.and_then(|s| s.peak_memory_bytes));
    let _ = value.setattr("fuel_consumed", stats.and_then(|s| s.fuel_consumed));
}

fn kind_to_py(kind: eryx::ErrorKind) -> PyErr {
    match kind {
        eryx::ErrorKind::Initialization(msg) => InitializationError::new_err(msg),
        eryx::ErrorKind::WasmEngine(msg) => InitializationError::new_err(msg),
        eryx::ErrorKind::EpochTicker(msg) => ExecutionError::new_err(msg),
        eryx::ErrorKind::WasmComponent(e) => InitializationError::new_err(e.to_string()),
        err @ eryx::ErrorKind::IncompatiblePrecompiled { .. } => {
            InitializationError::new_err(err.to_string())
        }
        eryx::ErrorKind::Execution(msg) => ExecutionError::new_err(msg),
        // A subclass of ExecutionError, so callers can tell a script error from
        // a sandbox failure while `except ExecutionError` still catches both.
        // `stdout` and `stderr` carry what the script wrote before it raised.
        eryx::ErrorKind::PythonException {
            traceback,
            stdout,
            stderr,
            ..
        } => {
            let err = PythonExceptionError::new_err(traceback);
            Python::attach(|py| {
                let value = err.value(py);
                // Setting an attribute on a fresh exception instance cannot
                // realistically fail; if it does, the traceback still raises.
                let _ = value.setattr("stdout", PyBytes::new(py, &stdout));
                let _ = value.setattr("stderr", PyBytes::new(py, &stderr));
            });
            err
        }
        eryx::ErrorKind::Callback(e) => ExecutionError::new_err(e.to_string()),
        eryx::ErrorKind::ResourceLimit(msg) => ResourceLimitError::new_err(msg),
        eryx::ErrorKind::Timeout(duration) => {
            TimeoutError::new_err(format!("execution timed out after {duration:?}"))
        }
        eryx::ErrorKind::Serialization(msg) => PyValueError::new_err(msg),
        eryx::ErrorKind::MissingPythonStdlib => InitializationError::new_err(
            "Python stdlib not found. Use Sandbox() which includes embedded runtime.",
        ),
        eryx::ErrorKind::Snapshot(msg) => InitializationError::new_err(msg),
        eryx::ErrorKind::Cancelled => CancelledError::new_err("execution was cancelled"),
        eryx::ErrorKind::Suspended(reason) => {
            ExecutionError::new_err(format!("execution suspended: {reason}"))
        }
        eryx::ErrorKind::FuelExhausted {
            consumed, limit, ..
        } => ResourceLimitError::new_err(format!(
            "execution ran out of fuel after {consumed} instructions (limit: {limit})"
        )),
        eryx::ErrorKind::Io(e) => PyRuntimeError::new_err(e.to_string()),
        // `eryx::Error` is `#[non_exhaustive]`; map any future variants to a
        // generic Eryx error rather than failing to compile.
        other => EryxError::new_err(other.to_string()),
    }
}

/// Get the process-wide Tokio runtime shared by every sandbox, session,
/// factory and MCP manager, creating it on first use.
///
/// The runtime is per process: a child created with `os.fork()` has none of
/// the parent runtime's worker threads, so the first call in the child builds
/// a fresh runtime. Objects created before the fork still hold the parent's
/// runtime and must not be used in the child; create new ones there.
pub fn make_runtime() -> PyResult<Arc<tokio::runtime::Runtime>> {
    static RUNTIME: Mutex<Option<(u32, Arc<tokio::runtime::Runtime>)>> = Mutex::new(None);
    let pid = std::process::id();
    let mut slot = RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.take() {
        Some((owner, runtime)) if owner == pid => {
            *slot = Some((owner, Arc::clone(&runtime)));
            return Ok(runtime);
        }
        // Inherited from the parent across a fork. Dropping it would wait for
        // worker threads that don't exist in this process, so leak it.
        Some((_, stale)) => std::mem::forget(stale),
        None => {}
    }
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| InitializationError::new_err(format!("failed to create runtime: {e}")))?,
    );
    *slot = Some((pid, Arc::clone(&runtime)));
    Ok(runtime)
}

/// Convert an `eryx::PoolError` to a Python exception.
pub fn pool_error_to_py(err: eryx::PoolError) -> PyErr {
    match err {
        eryx::PoolError::Timeout(d) => {
            PoolTimeoutError::new_err(format!("acquire timed out after {d:?}"))
        }
        eryx::PoolError::Closed => PoolClosedError::new_err("pool is closed"),
        eryx::PoolError::Creation(msg) => InitializationError::new_err(msg),
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
    m.add("TimeoutError", m.py().get_type::<TimeoutError>())?;
    m.add("CancelledError", m.py().get_type::<CancelledError>())?;
    m.add("SuspendCallback", m.py().get_type::<SuspendCallback>())?;
    m.add("PoolError", m.py().get_type::<PoolError>())?;
    m.add("PoolTimeoutError", m.py().get_type::<PoolTimeoutError>())?;
    m.add("PoolClosedError", m.py().get_type::<PoolClosedError>())?;
    Ok(())
}
