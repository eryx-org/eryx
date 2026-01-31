//! CPython FFI bindings for eryx-wasm-runtime.
//!
//! This module re-exports pyo3::ffi for CPython C API bindings where available
//! in the stable ABI (abi3), with manual declarations for functions not exposed there.
//!
//! These symbols are resolved at component link time when we link against libpython.

#![allow(missing_docs)]
#![allow(missing_debug_implementations)]

use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};

// Re-export pyo3::ffi types and functions available in the stable ABI
pub use pyo3::ffi::{
    // Reference counting
    Py_DecRef,
    // Interpreter lifecycle
    Py_FinalizeEx,
    Py_IncRef,
    Py_Initialize,
    Py_InitializeEx,
    Py_IsInitialized,
    // Bytes operations
    PyBytes_AsString,
    PyBytes_AsStringAndSize,
    PyBytes_FromStringAndSize,
    PyBytes_Size,
    // Dict operations
    PyDict_Clear,
    PyDict_Copy,
    PyDict_GetItem,
    PyDict_GetItemString,
    PyDict_New,
    PyDict_SetItem,
    PyDict_SetItemString,
    PyDict_Update,
    // Exception handling
    PyErr_Clear,
    PyErr_GetRaisedException,
    PyErr_Occurred,
    PyErr_Print,
    PyErr_PrintEx,
    PyErr_SetString,
    // Exception types
    PyExc_AttributeError,
    PyExc_BaseException,
    PyExc_Exception,
    PyExc_IndexError,
    PyExc_KeyError,
    PyExc_MemoryError,
    PyExc_RuntimeError,
    PyExc_SystemExit,
    PyExc_TypeError,
    PyExc_ValueError,
    // Module operations
    PyImport_AddModule,
    PyImport_AppendInittab,
    PyImport_ImportModule,
    // List operations
    PyList_Append,
    PyList_New,
    // Long (int) operations
    PyLong_AsLong,
    PyLong_FromLong,
    PyModule_AddObject,
    PyModule_AddObjectRef,
    PyModule_GetDict,
    // Core type
    PyObject,
    // Object protocol
    PyObject_Call,
    PyObject_CallNoArgs,
    PyObject_GetAttrString,
    PyObject_Repr,
    PyObject_SetAttrString,
    PyObject_Str,
    // Tuple operations
    PyTuple_GetItem,
    PyTuple_New,
    PyTuple_SetItem,
    PyTuple_Size,
    // String/Unicode operations
    PyUnicode_AsUTF8AndSize,
    PyUnicode_FromString,
    PyUnicode_FromStringAndSize,
};

// Functions not available in pyo3-ffi stable ABI (abi3)
unsafe extern "C" {
    pub fn PyRun_SimpleString(command: *const c_char) -> std::ffi::c_int;
    pub fn PyUnicode_AsUTF8(unicode: *mut PyObject) -> *const c_char;
}

// =============================================================================
// Invoke callback mechanism
// =============================================================================

/// Type for the invoke callback function.
/// Takes (name, args_json) and returns Result<result_json, error_message>.
pub type InvokeCallback = fn(&str, &str) -> Result<String, String>;

/// Type for the async invoke callback function.
/// Takes (name, args_json) and returns InvokeResult (Ok/Err/Pending).
pub type InvokeAsyncCallback = fn(&str, &str) -> Result<crate::InvokeResult, String>;

/// Type for the report_trace callback function.
/// Takes (lineno, event_json, context_json) and sends to host.
pub type ReportTraceCallback = fn(u32, &str, &str);

use std::cell::RefCell;

// Thread-local storage for the callbacks.
// These are set by lib.rs during export execution.
// Note: WASM is single-threaded, so RefCell is sufficient and avoids lock overhead.
thread_local! {
    static INVOKE_CALLBACK: RefCell<Option<InvokeCallback>> = const { RefCell::new(None) };
    static INVOKE_ASYNC_CALLBACK: RefCell<Option<InvokeAsyncCallback>> = const { RefCell::new(None) };
    static REPORT_TRACE_CALLBACK: RefCell<Option<ReportTraceCallback>> = const { RefCell::new(None) };
    /// Stores the last callback error if Python callback execution failed.
    /// This allows export_async_callback to detect uncaught exceptions.
    static LAST_CALLBACK_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the invoke callback function.
/// This should be called by lib.rs before executing Python code.
pub fn set_invoke_callback(callback: Option<InvokeCallback>) {
    INVOKE_CALLBACK.with(|cell| *cell.borrow_mut() = callback);
}

/// Set the async invoke callback function.
/// This should be called by lib.rs before executing Python code.
pub fn set_invoke_async_callback(callback: Option<InvokeAsyncCallback>) {
    INVOKE_ASYNC_CALLBACK.with(|cell| *cell.borrow_mut() = callback);
}

/// Set the report_trace callback function.
/// This should be called by lib.rs before executing Python code.
pub fn set_report_trace_callback(callback: Option<ReportTraceCallback>) {
    REPORT_TRACE_CALLBACK.with(|cell| *cell.borrow_mut() = callback);
}

/// Get and clear the last callback error, if any.
/// This is called by export_async_callback to detect uncaught Python exceptions.
pub fn take_last_callback_error() -> Option<String> {
    LAST_CALLBACK_ERROR.with(|cell| cell.borrow_mut().take())
}

/// Store a callback error.
fn set_last_callback_error(error: String) {
    LAST_CALLBACK_ERROR.with(|cell| *cell.borrow_mut() = Some(error));
}

/// Call the registered invoke callback.
/// Returns Err if no callback is registered or if the callback fails.
pub fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    INVOKE_CALLBACK.with(|cell| {
        let callback = cell.borrow();
        let callback = callback.as_ref().ok_or_else(|| {
            "invoke() called outside of execute context - callbacks can only be called during code execution".to_string()
        })?;
        callback(name, args_json)
    })
}

/// Call the registered async invoke callback.
/// Returns the InvokeResult (Ok/Err/Pending) or an error if no callback is registered.
pub fn do_invoke_async(name: &str, args_json: &str) -> Result<crate::InvokeResult, String> {
    INVOKE_ASYNC_CALLBACK.with(|cell| {
        let callback = cell.borrow();
        let callback = callback.as_ref().ok_or_else(|| {
            "invoke() called outside of execute context - callbacks can only be called during code execution".to_string()
        })?;
        callback(name, args_json)
    })
}

/// Call the registered report_trace callback.
/// Silently does nothing if no callback is registered (tracing disabled).
pub fn do_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    REPORT_TRACE_CALLBACK.with(|cell| {
        let callback = cell.borrow();
        if let Some(cb) = callback.as_ref() {
            cb(lineno, event_json, context_json);
        }
        // If no callback registered, tracing is simply disabled - not an error
    });
}

// =============================================================================
// Execute result type for async support
// =============================================================================

/// Callback codes from the Component Model async protocol (used by _eryx_async)
pub mod callback_code {
    pub const EXIT: u32 = 0;
    pub const YIELD: u32 = 1;
    pub const WAIT: u32 = 2;
    pub const POLL: u32 = 3;

    /// Extract the callback code from a first_poll/callback return value.
    pub fn get_code(value: u32) -> u32 {
        value & 0xF
    }

    /// Extract the waitable_set handle from a WAIT return value.
    pub fn get_waitable_set(value: u32) -> u32 {
        value >> 4
    }
}

/// Output from executing Python code.
#[derive(Debug, Clone)]
pub struct ExecuteOutput {
    /// Captured stdout from the Python execution.
    pub stdout: String,
    /// Captured stderr from the Python execution.
    pub stderr: String,
}

/// Result of executing Python code.
#[derive(Debug)]
pub enum ExecuteResult {
    /// Execution completed successfully with output (stdout and stderr).
    Complete(ExecuteOutput),
    /// Execution completed with an error.
    Error(String),
    /// Execution is pending, waiting for async callback.
    /// Contains the raw callback code from first_poll (WAIT | waitable_set << 4).
    Pending(u32),
}

/// Read the `_eryx_callback_code` global variable from Python.
///
/// This is set by `_eryx_run_async` after calling `first_poll`.
/// Returns 0 (EXIT) if the variable doesn't exist or can't be read.
fn get_python_callback_code() -> u32 {
    unsafe {
        let main_module = PyImport_AddModule(c"__main__".as_ptr());
        if main_module.is_null() {
            return 0;
        }

        let main_dict = PyModule_GetDict(main_module);
        if main_dict.is_null() {
            return 0;
        }

        let var_name = c"_eryx_callback_code";
        let var = PyDict_GetItemString(main_dict, var_name.as_ptr());
        if var.is_null() {
            return 0;
        }

        // Get the integer value
        let value = PyLong_AsLong(var);
        if value < 0 && !PyErr_Occurred().is_null() {
            PyErr_Clear();
            return 0;
        }

        value as u32
    }
}

/// Call Python's `_eryx_async.resume(event0, event1, event2)`.
///
/// This is called from `export_async_callback` to resume a suspended async operation.
/// Returns the callback code (EXIT or WAIT | waitable_set).
///
/// If Python raises an uncaught exception, the error is stored and can be retrieved
/// with `take_last_callback_error()`.
pub fn call_python_callback(event0: u32, event1: u32, event2: u32) -> u32 {
    use std::ffi::CString;

    let code = format!(
        r#"
import _eryx_async
_eryx_callback_code = _eryx_async.resume({event0}, {event1}, {event2})
"#
    );

    let code_cstr = match CString::new(code) {
        Ok(s) => s,
        Err(_) => return callback_code::EXIT,
    };

    unsafe {
        if PyRun_SimpleString(code_cstr.as_ptr()) != 0 {
            // Python callback raised an uncaught exception.
            // Capture the error before clearing it.
            let error = get_last_error_message();
            set_last_callback_error(error);
            PyErr_Clear();
            return callback_code::EXIT;
        }
    }

    get_python_callback_code()
}

/// Re-capture stdout after async execution completes.
///
/// This is called from `export_async_callback` when the async execution finishes.
/// When execution returned Pending, we kept the capture variables (_eryx_stdout, etc.)
/// with stdout still redirected so that async code could still print. Now we capture
/// the final output and restore original stdout/stderr.
pub fn recapture_stdout() {
    // Capture the output and restore original stdout/stderr using our helper
    unsafe {
        if PyRun_SimpleString(c"_eryx_output, _eryx_errors = _eryx_get_output()".as_ptr()) != 0 {
            PyErr_Clear();
        }
    }
}

/// Store the result of an async import for Python's promise_get_result to read.
///
/// This is called from `export_async_callback` after lifting the result from the buffer.
/// The result is stored in `_eryx_async_import_results[subtask]` in Python.
///
/// We use a dict keyed by subtask ID to avoid race conditions when multiple
/// async callbacks are in flight (e.g., with asyncio.gather()).
pub fn set_async_import_result(subtask: u32, result_json: &str) {
    use std::ffi::CString;

    // Escape the JSON for embedding in Python triple-quoted string.
    // We need to escape backslashes first, then single quotes, then handle
    // potential triple-quote sequences.
    let escaped = result_json.replace('\\', "\\\\").replace("'''", "\\'''");

    // Store in a dict keyed by subtask ID to support concurrent callbacks
    let code = format!("_eryx_async_import_results[{subtask}] = '''{escaped}'''");

    if let Ok(code_cstr) = CString::new(code) {
        unsafe {
            if PyRun_SimpleString(code_cstr.as_ptr()) != 0 {
                PyErr_Clear();
            }
        }
    }
}

/// Store the result of a TLS async operation for Python to retrieve.
///
/// - `subtask`: The subtask ID (used as key)
/// - `status`: 0 = Ok, 1 = Error
/// - `value`: For Ok: the handle/u32 value. For Error: error discriminant.
/// - `message`: Optional error message for errors.
pub fn set_net_result(subtask: u32, status: i32, value: i64, message: Option<String>) {
    use std::ffi::CString;

    let code = match (status, message) {
        (0, _) => format!("_eryx_net_results[{subtask}] = (0, {value})"),
        (1, Some(msg)) => {
            let escaped = msg.replace('\\', "\\\\").replace("'''", "\\'''");
            format!("_eryx_net_results[{subtask}] = (1, '''{escaped}''')")
        }
        (1, None) => format!("_eryx_net_results[{subtask}] = (1, 'unknown error')"),
        _ => return,
    };

    if let Ok(code_cstr) = CString::new(code) {
        unsafe {
            if PyRun_SimpleString(code_cstr.as_ptr()) != 0 {
                PyErr_Clear();
            }
        }
    }
}

/// Store the result of a network read operation (bytes result).
pub fn set_net_bytes_result(subtask: u32, status: i32, data: Vec<u8>) {
    use std::ffi::CString;

    let code = if status == 0 {
        // Encode bytes as a Python bytes literal
        let hex: String = data.iter().map(|b| format!("\\x{b:02x}")).collect();
        format!("_eryx_net_results[{subtask}] = (0, b'{hex}')")
    } else {
        format!("_eryx_net_results[{subtask}] = (1, 'unknown error')")
    };

    if let Ok(code_cstr) = CString::new(code) {
        unsafe {
            if PyRun_SimpleString(code_cstr.as_ptr()) != 0 {
                PyErr_Clear();
            }
        }
    }
}

// =============================================================================
// _eryx built-in module (PyO3-based)
// =============================================================================
//
// This module provides the low-level `_eryx_invoke` function that Python code
// uses to call host callbacks. It uses PyO3 macros to generate a proper C
// extension module that works correctly in WASM.
//
// Note: We use PyO3 instead of manual CPython FFI because manual PyModuleDef
// structures have WASM memory compatibility issues - Python can't read memory
// allocated by our .so module.

use pyo3::prelude::*;

/// Low-level invoke function exposed to Python.
/// Python signature: _eryx_invoke(name: str, args_json: str) -> str
#[pyfunction]
fn _eryx_invoke(name: String, args_json: String) -> PyResult<String> {
    match do_invoke(&name, &args_json) {
        Ok(result) => Ok(result),
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Report a trace event to the host.
/// This is called by sys.settrace to report line/call/return/exception events.
/// Python signature: _eryx_report_trace(lineno: int, event_json: str, context_json: str) -> None
#[pyfunction]
fn _eryx_report_trace(lineno: u32, event_json: String, context_json: String) {
    do_report_trace(lineno, &event_json, &context_json);
}

/// Async-aware invoke function exposed to Python.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the JSON result string
/// - result_type 1: Err - value is the error message string
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_invoke_async(name: str, args_json: str) -> tuple[int, Any]
#[pyfunction]
fn _eryx_invoke_async(
    py: Python<'_>,
    name: String,
    args_json: String,
) -> PyResult<(i32, Py<PyAny>)> {
    match do_invoke_async(&name, &args_json) {
        Ok(crate::InvokeResult::Ok(result)) => {
            Ok((0, result.into_pyobject(py)?.into_any().unbind()))
        }
        Ok(crate::InvokeResult::Err(error)) => {
            Ok((1, error.into_pyobject(py)?.into_any().unbind()))
        }
        Ok(crate::InvokeResult::Pending(waitable, promise)) => {
            let tuple = (waitable, promise);
            Ok((2, tuple.into_pyobject(py)?.into_any().unbind()))
        }
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

// =============================================================================
// Async support FFI functions
// =============================================================================
//
// These functions expose Component Model async intrinsics to Python.
// They are used by the embedded _eryx_async module.

/// Create a new waitable set for tracking pending async operations.
/// Returns the waitable set handle.
#[pyfunction]
fn waitable_set_new_() -> u32 {
    unsafe { crate::waitable_set_new() }
}

/// Drop a waitable set when no longer needed.
#[pyfunction]
fn waitable_set_drop_(set: u32) {
    unsafe { crate::waitable_set_drop(set) }
}

/// Add a waitable (subtask) to a waitable set for polling.
#[pyfunction]
fn waitable_join_(waitable: u32, set: u32) {
    unsafe { crate::waitable_join(waitable, set) }
}

/// Store a context value (Python object) for async resumption.
/// The object reference count is incremented to keep it alive.
#[pyfunction]
fn context_set_(value: Option<Py<PyAny>>) {
    let ptr = match value {
        Some(obj) => {
            // Increment ref count so it stays alive while stored
            let ptr = obj.as_ptr();
            unsafe {
                pyo3::ffi::Py_IncRef(ptr);
            }
            ptr as u32
        }
        None => 0,
    };
    unsafe { crate::context_set(ptr) }
}

/// Retrieve the stored context value.
/// Returns None if no context was stored (ptr was 0).
#[pyfunction]
fn context_get_(py: Python<'_>) -> Option<Py<PyAny>> {
    let ptr = unsafe { crate::context_get() };
    if ptr == 0 {
        None
    } else {
        // Convert raw pointer back to PyObject
        // The object was incref'd when stored, so we borrow it here
        let obj_ptr = ptr as *mut pyo3::ffi::PyObject;
        // Create a Py<PyAny> from the raw pointer (steals reference)
        Some(unsafe { Py::from_borrowed_ptr(py, obj_ptr) })
    }
}

/// Drop a completed subtask to release resources.
#[pyfunction]
fn subtask_drop_(task: u32) {
    unsafe { crate::subtask_drop(task) }
}

/// Get result from a completed async promise.
///
/// This retrieves the result JSON stored in `__main__._eryx_async_import_results[subtask]`
/// when the Rust layer completed an async import callback.
///
/// The subtask ID is used to look up the correct result when multiple callbacks
/// are in flight concurrently.
#[pyfunction]
fn promise_get_result_(py: Python<'_>, subtask: u32) -> PyResult<String> {
    // Get the result from __main__._eryx_async_import_results[subtask]
    let main_module = py.import("__main__")?;
    match main_module.getattr("_eryx_async_import_results") {
        Ok(results_dict) => {
            // Try to pop the result (removes it from the dict)
            match results_dict.call_method1("pop", (subtask, py.None())) {
                Ok(result) => {
                    if result.is_none() {
                        Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "Async import result not found for subtask {subtask}"
                        )))
                    } else {
                        result.extract()
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(_) => {
            // Dict not found - this means initialization failed
            Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Async import results dict not available - initialization may have failed",
            ))
        }
    }
}

// =============================================================================
// TCP functions exposed to Python
// =============================================================================
//
// These functions call the WIT TCP interface for networking.
// They are used by the socket shim for plain HTTP connections.

/// Connect to a host:port over TCP.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the TCP handle (int)
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tcp_connect(host: str, port: int) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tcp_connect(py: Python<'_>, host: String, port: u16) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tcp_connect(&host, port) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Handle(h) => {
                Ok((status, h.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Bytes(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected bytes result from connect",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Read from a TCP connection.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the bytes read
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tcp_read(handle: int, length: int) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tcp_read(py: Python<'_>, handle: u32, length: u32) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tcp_read(handle, length) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Bytes(b) => {
                Ok((status, b.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Handle(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected handle result from read",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Write to a TCP connection.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the number of bytes written (int)
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tcp_write(handle: int, data: bytes) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tcp_write(py: Python<'_>, handle: u32, data: Vec<u8>) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tcp_write(handle, &data) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Handle(n) => {
                Ok((status, n.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Bytes(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected bytes result from write",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Close a TCP connection.
/// Python signature: _eryx_tcp_close(handle: int) -> None
#[pyfunction]
fn _eryx_tcp_close(handle: u32) {
    crate::do_tcp_close(handle);
}

// =============================================================================
// TLS functions exposed to Python
// =============================================================================
//
// These functions call the WIT TLS interface for networking.
// TLS upgrade takes a TCP handle and returns a TLS handle.

/// Upgrade a TCP connection to TLS.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the TLS handle (int)
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tls_upgrade(tcp_handle: int, hostname: str) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tls_upgrade(
    py: Python<'_>,
    tcp_handle: u32,
    hostname: String,
) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tls_upgrade(tcp_handle, &hostname) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Handle(h) => {
                Ok((status, h.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Bytes(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected bytes result from tls upgrade",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Read from a TLS connection.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the bytes read
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tls_read(handle: int, length: int) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tls_read(py: Python<'_>, handle: u32, length: u32) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tls_read(handle, length) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Bytes(b) => {
                Ok((status, b.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Handle(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected handle result from read",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Write to a TLS connection.
/// Returns a tuple: (result_type, value)
/// - result_type 0: Ok - value is the number of bytes written (int)
/// - result_type 1: Err - value is the error message (str)
/// - result_type 2: Pending - value is a tuple (waitable_id, promise_id)
///
/// Python signature: _eryx_tls_write(handle: int, data: bytes) -> tuple[int, Any]
#[pyfunction]
fn _eryx_tls_write(py: Python<'_>, handle: u32, data: Vec<u8>) -> PyResult<(i32, Py<PyAny>)> {
    match crate::do_tls_write(handle, &data) {
        Ok((status, value)) => match value {
            crate::NetResultValue::Handle(n) => {
                Ok((status, n.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Error(e) => {
                Ok((status, e.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Pending(waitable, promise) => {
                let tuple = (waitable, promise);
                Ok((status, tuple.into_pyobject(py)?.into_any().unbind()))
            }
            crate::NetResultValue::Bytes(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
                "unexpected bytes result from write",
            )),
        },
        Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e)),
    }
}

/// Close a TLS connection.
/// Python signature: _eryx_tls_close(handle: int) -> None
#[pyfunction]
fn _eryx_tls_close(handle: u32) {
    crate::do_tls_close(handle);
}

/// The _eryx module definition.
/// This generates a `PyInit__eryx` function that can be registered with Python.
#[pymodule]
#[pyo3(name = "_eryx")]
fn eryx_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_eryx_invoke, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_invoke_async, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_report_trace, m)?)?;
    // Async support functions
    m.add_function(wrap_pyfunction!(waitable_set_new_, m)?)?;
    m.add_function(wrap_pyfunction!(waitable_set_drop_, m)?)?;
    m.add_function(wrap_pyfunction!(waitable_join_, m)?)?;
    m.add_function(wrap_pyfunction!(context_set_, m)?)?;
    m.add_function(wrap_pyfunction!(context_get_, m)?)?;
    m.add_function(wrap_pyfunction!(subtask_drop_, m)?)?;
    m.add_function(wrap_pyfunction!(promise_get_result_, m)?)?;
    // TCP networking functions
    m.add_function(wrap_pyfunction!(_eryx_tcp_connect, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tcp_read, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tcp_write, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tcp_close, m)?)?;
    // TLS networking functions
    m.add_function(wrap_pyfunction!(_eryx_tls_upgrade, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tls_read, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tls_write, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_tls_close, m)?)?;
    Ok(())
}

// =============================================================================
// Embedded _eryx_async module
// =============================================================================
//
// This is eryx's native async runtime for Python. It provides a minimal
// asyncio event loop implementation for the Component Model's async callback
// protocol.

/// Python code to inject the _eryx_async module into sys.modules.
///
/// This creates a new module object and executes the module code in its
/// namespace, then registers it so `import _eryx_async` works.
///
/// The module provides:
/// - `_EryxLoop`: Minimal asyncio event loop for Component Model async
/// - `run_async(coro)`: Entry point for async execution, returns callback code
/// - `resume(e0, e1, e2)`: Resume suspended execution after callback completes
/// - `await_invoke(name, args_json)`: Invoke a callback and await its result
const ERYX_ASYNC_INJECT_CODE: &str = r#"
import sys, types

_eryx_async = types.ModuleType('_eryx_async')
_eryx_async.__doc__ = 'Eryx async runtime - minimal asyncio event loop for Component Model async.'

# Note: _eryx_async_import_results is initialized in ERYX_EXEC_INFRASTRUCTURE
# (which runs after this) to ensure it's in __main__ for PyO3 getattr to find it.

exec(compile(r'''
"""Eryx async runtime - minimal asyncio event loop for Component Model async."""

import asyncio
import _eryx
from contextvars import ContextVar, Context
from dataclasses import dataclass
from typing import Any, Optional

# Callback codes (returned from run_async/resume)
EXIT = 0   # Execution complete
WAIT = 2   # Execution pending, need to wait

# Event types (from Component Model)
_EVENT_NONE = 0
_EVENT_SUBTASK = 1

# Status codes (for subtask events)
_STATUS_RETURNED = 2

@dataclass
class _AsyncState:
    """State for tracking pending async operations."""
    waitable_set: Optional[int]
    futures: dict[int, asyncio.Future]
    handles: list[asyncio.Handle]
    pending_count: int


class _EryxLoop(asyncio.AbstractEventLoop):
    """Minimal event loop for Component Model async.

    Only implements the methods actually needed by asyncio.Task and our
    async runtime. All other methods raise NotImplementedError.
    """

    def __init__(self):
        self.running = True
        self.exception = None

    def poll(self, state: _AsyncState):
        """Run pending callbacks until none remain."""
        while True:
            handles, state.handles = state.handles, []
            for h in handles:
                if not h._cancelled:
                    h._run()
            if self.exception is not None:
                raise self.exception
            if not handles and not state.handles:
                return

    # Required methods for asyncio.Task to work
    def call_soon(self, callback, *args, context=None):
        if context is None:
            raise AssertionError("context required for call_soon")
        handle = asyncio.Handle(callback, args, self, context)
        context[_async_state].handles.append(handle)
        return handle

    def create_task(self, coro, *, name=None, context=None):
        return asyncio.Task(coro, loop=self, context=context)

    def create_future(self):
        return asyncio.Future(loop=self)

    def is_running(self):
        return self.running

    def is_closed(self):
        return not self.running

    def get_debug(self):
        return False

    def call_exception_handler(self, context):
        self.exception = context.get('exception')

    # Stub methods required by AbstractEventLoop
    def time(self): raise NotImplementedError
    def run_forever(self): raise NotImplementedError
    def run_until_complete(self, future): raise NotImplementedError
    def stop(self): self.running = False
    def close(self): self.running = False
    def shutdown_asyncgens(self): return _noop()


async def _noop():
    pass


# Module-level state
_async_state: ContextVar[_AsyncState] = ContextVar("_async_state")
_loop = _EryxLoop()
asyncio.set_event_loop(_loop)
_loop.running = True
asyncio.events._set_running_loop(_loop)


def _set_async_state(state: _AsyncState) -> None:
    _async_state.set(state)


async def _return_result(coroutine: Any) -> None:
    """Wrapper that decrements pending_count when coroutine completes."""
    global _async_state
    try:
        await coroutine
    except Exception as e:
        _loop.exception = e
    _async_state.get().pending_count -= 1


def run_async(coro) -> int:
    """Run a coroutine, return callback code (EXIT or WAIT|waitable_set<<4)."""
    ctx = Context()
    state = _AsyncState(None, {}, [], 1)
    ctx.run(_set_async_state, state)
    asyncio.create_task(_return_result(coro), context=ctx)
    return _poll(state)


def resume(event0: int, event1: int, event2: int) -> int:
    """Resume suspended execution after callback completes."""
    state = _eryx.context_get_()
    _eryx.context_set_(None)

    # Handle subtask completion
    if event0 == _EVENT_SUBTASK and event2 == _STATUS_RETURNED:
        _eryx.waitable_join_(event1, 0)
        _eryx.subtask_drop_(event1)
        state.futures.pop(event1).set_result(event2)
    elif event0 == _EVENT_NONE:
        pass  # Just poll again
    # Other events (streams, futures) would go here if we supported them

    return _poll(state)


def _poll(state: _AsyncState) -> int:
    """Poll the event loop and return callback code."""
    _loop.poll(state)
    if state.pending_count == 0:
        if state.waitable_set is not None:
            _eryx.waitable_set_drop_(state.waitable_set)
        return EXIT
    else:
        assert state.waitable_set is not None, "pending but no waitable_set"
        _eryx.context_set_(state)
        return WAIT | (state.waitable_set << 4)


async def await_invoke(name: str, args_json: str) -> str:
    """Invoke a callback and await its result. Returns result JSON."""
    import json

    result_type, value = _eryx._eryx_invoke_async(name, args_json)

    if result_type == 0:  # Ok - immediate completion
        return value
    elif result_type == 1:  # Err - immediate error
        raise RuntimeError(value)
    else:  # Pending - need to wait
        waitable, promise = value
        future = _loop.create_future()
        state = _async_state.get()
        state.futures[waitable] = future

        if state.waitable_set is None:
            state.waitable_set = _eryx.waitable_set_new_()
        _eryx.waitable_join_(waitable, state.waitable_set)

        await future

        # Get the result wrapper and parse it.
        # Use waitable (the subtask ID) as the key, not promise (which is always 0).
        result_json = _eryx.promise_get_result_(waitable)
        try:
            result = json.loads(result_json)
        except json.JSONDecodeError as e:
            raise RuntimeError(f"Failed to parse callback result JSON: {e}. Raw: {result_json[:200]}") from e
        if result.get('ok', False):
            # Return the value as a JSON string
            value = result.get('value', '')
            # Always JSON-encode so invoke() can call json.loads()
            return json.dumps(value)
        else:
            error = result.get('error')
            if error is None:
                raise RuntimeError(f"Callback failed with no error message. Result: {result}")
            raise RuntimeError(error)


async def _await_net_result(result_type: int, value: Any) -> Any:
    """Helper to await a network (TCP or TLS) operation result."""
    import __main__
    if result_type == 0:  # Ok - immediate completion
        return value
    elif result_type == 1:  # Err - immediate error
        raise OSError(value)
    else:  # Pending - need to wait
        waitable, _promise = value
        future = _loop.create_future()
        state = _async_state.get()
        state.futures[waitable] = future

        if state.waitable_set is None:
            state.waitable_set = _eryx.waitable_set_new_()
        _eryx.waitable_join_(waitable, state.waitable_set)

        await future

        # Get result from _eryx_net_results dict (set by Rust during resume)
        net_results = getattr(__main__, '_eryx_net_results', {})
        if waitable in net_results:
            status, result_value = net_results.pop(waitable)
            if status == 0:
                return result_value
            else:
                raise OSError(result_value)
        else:
            raise OSError("Network result not found")


# TCP functions
async def await_tcp_connect(host: str, port: int) -> int:
    """Connect to a host over TCP and return the handle."""
    result_type, value = _eryx._eryx_tcp_connect(host, port)
    return await _await_net_result(result_type, value)


async def await_tcp_read(handle: int, length: int) -> bytes:
    """Read from a TCP connection."""
    result_type, value = _eryx._eryx_tcp_read(handle, length)
    return await _await_net_result(result_type, value)


async def await_tcp_write(handle: int, data: bytes) -> int:
    """Write to a TCP connection, return bytes written."""
    result_type, value = _eryx._eryx_tcp_write(handle, data)
    return await _await_net_result(result_type, value)


def tcp_close(handle: int):
    """Close a TCP connection."""
    _eryx._eryx_tcp_close(handle)


# TLS functions (upgrade from TCP)
async def await_tls_upgrade(tcp_handle: int, hostname: str) -> int:
    """Upgrade a TCP connection to TLS and return the TLS handle."""
    result_type, value = _eryx._eryx_tls_upgrade(tcp_handle, hostname)
    return await _await_net_result(result_type, value)


async def await_tls_read(handle: int, length: int) -> bytes:
    """Read from a TLS connection."""
    result_type, value = _eryx._eryx_tls_read(handle, length)
    return await _await_net_result(result_type, value)


async def await_tls_write(handle: int, data: bytes) -> int:
    """Write to a TLS connection, return bytes written."""
    result_type, value = _eryx._eryx_tls_write(handle, data)
    return await _await_net_result(result_type, value)


def tls_close(handle: int):
    """Close a TLS connection."""
    _eryx._eryx_tls_close(handle)
''', '_eryx_async', 'exec'), _eryx_async.__dict__)

sys.modules['_eryx_async'] = _eryx_async
"#;

// =============================================================================
// Python interpreter state
// =============================================================================

/// Track whether we've initialized Python.
static PYTHON_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize Python interpreter.
///
/// This should be called once during `wit_dylib_initialize`.
/// Subsequent calls are no-ops.
///
/// Prerequisites:
/// - PYTHONPATH environment variable must be set to include stdlib paths
///   (e.g., "/python-stdlib:/site-packages")
/// - WASI preopened directories must be configured for those paths
///
/// Sets up:
/// - The _eryx built-in module (for invoke() callback support)
/// - Python interpreter (without signal handlers - not useful in WASM)
/// - The _eryx_async module (embedded async runtime)
/// - Ensures __main__ module exists for code execution
///
/// # `ERYX_EXEC_INFRASTRUCTURE`
///
/// Python code to set up execution infrastructure.
/// This is run ONCE during `initialize_python()` and sets up:
/// - Persistent stdout/stderr capture (reused across executions)
/// - Socket patching for asyncio compatibility
/// - The _eryx_exec() function that runs user code efficiently
/// - User globals namespace (isolated from infrastructure)
const ERYX_EXEC_INFRASTRUCTURE: &str = r#"
import sys as _sys
from io import StringIO as _StringIO
import ast as _ast
import types as _types

# Patch socket.socketpair before importing asyncio
# WASI doesn't support socketpair, so we create a dummy that works for asyncio's self-pipe
import socket as _socket

class _DummySocket:
    '''Dummy socket for asyncio self-pipe in WASI.'''
    def __init__(self):
        self._buffer = []
        self._closed = False
    def fileno(self):
        return -1
    def setblocking(self, flag):
        pass
    def send(self, data):
        self._buffer.append(data)
        return len(data)
    def recv(self, n):
        if self._buffer:
            return self._buffer.pop(0)
        return b''
    def close(self):
        self._closed = True
    def __enter__(self):
        return self
    def __exit__(self, *args):
        self.close()

def _dummy_socketpair(family=None, type=None, proto=0):
    return (_DummySocket(), _DummySocket())

_socket.socketpair = _dummy_socketpair

# Persistent stdout/stderr capture - created once, reused across executions
_eryx_stdout = _StringIO()
_eryx_stderr = _StringIO()
_eryx_old_stdout = _sys.stdout
_eryx_old_stderr = _sys.stderr

# User globals namespace - isolated from infrastructure
# User code cannot see _eryx_* variables because they're in module globals, not here
_eryx_user_globals = {'__builtins__': __builtins__, '__name__': '__main__'}

# Network async results storage - keyed by subtask ID
# Used by set_net_result/set_net_bytes_result to store results for Python to retrieve
_eryx_net_results = {}

# Async import results storage - keyed by subtask ID
# Used by set_async_import_result/promise_get_result_ for concurrent callback safety
# This MUST be in __main__ for PyRun_SimpleString and PyO3 getattr to find it
_eryx_async_import_results = {}

# Import async infrastructure
import _eryx_async
import _eryx as _eryx_mod
import json as _json

# Callback code storage
_eryx_callback_code = 0

def _eryx_run_async(coro):
    '''Run a coroutine using _eryx_async runtime.'''
    global _eryx_callback_code
    _eryx_callback_code = _eryx_async.run_async(coro)
    return None

# Trace function for sys.settrace
def _eryx_trace_func(frame, event, arg):
    '''Trace function called by Python for each execution event.'''
    filename = frame.f_code.co_filename
    if filename != '<user>':
        return _eryx_trace_func

    lineno = frame.f_lineno
    func_name = frame.f_code.co_name

    if func_name.startswith('_') and func_name != '<module>':
        return _eryx_trace_func

    if event == 'line':
        _eryx_mod._eryx_report_trace(lineno, _json.dumps({"type": "line"}), "")
    elif event == 'call':
        _eryx_mod._eryx_report_trace(lineno, _json.dumps({"type": "call", "function": func_name}), "")
    elif event == 'return':
        _eryx_mod._eryx_report_trace(lineno, _json.dumps({"type": "return", "function": func_name}), "")
    elif event == 'exception':
        exc_type, exc_value, _ = arg
        if exc_type is StopIteration:
            return _eryx_trace_func
        _eryx_mod._eryx_report_trace(lineno, _json.dumps({
            "type": "exception",
            "exception_type": exc_type.__name__ if exc_type else "Unknown",
            "message": str(exc_value) if exc_value else ""
        }), "")

    return _eryx_trace_func

def _eryx_exec(code):
    '''Execute user code efficiently. Called once per execute().

    This function is pre-compiled and runs user code in an isolated namespace.
    Infrastructure variables (_eryx_*) are not visible to user code.
    '''
    global _eryx_callback_code
    _eryx_callback_code = 0

    # Clear and redirect stdout/stderr
    _eryx_stdout.seek(0)
    _eryx_stdout.truncate(0)
    _eryx_stderr.seek(0)
    _eryx_stderr.truncate(0)
    _sys.stdout = _eryx_stdout
    _sys.stderr = _eryx_stderr

    # Compile user code with top-level await support
    compiled = compile(code, '<user>', 'exec', flags=_ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)

    # Enable tracing
    _sys.settrace(_eryx_trace_func)

    try:
        # Check if the compiled code is a coroutine (has top-level await)
        if compiled.co_flags & 0x80:  # CO_COROUTINE
            fn = _types.FunctionType(compiled, _eryx_user_globals)
            coro = fn()
            _eryx_run_async(coro)
        else:
            # Regular synchronous code - execute in user namespace
            exec(compiled, _eryx_user_globals, _eryx_user_globals)
    finally:
        _sys.settrace(None)

def _eryx_get_output():
    '''Get captured stdout and restore original streams.'''
    _sys.stdout = _eryx_old_stdout
    _sys.stderr = _eryx_old_stderr
    return _eryx_stdout.getvalue(), _eryx_stderr.getvalue()

def _eryx_get_output_keep_capture():
    '''Get captured stdout without restoring streams (for pending async).'''
    return _eryx_stdout.getvalue(), _eryx_stderr.getvalue()
"#;

// =============================================================================
// Socket shim module for TLS networking
// =============================================================================
//
// This provides a minimal socket module replacement that works with HTTP client
// libraries like requests and httpx. The socket itself doesn't do networking -
// it just holds connection parameters until ssl.wrap_socket() is called.

// =============================================================================
// SSL shim module for TLS networking
// =============================================================================
//
// This provides a minimal ssl module replacement that performs TLS connections
// via the WIT TLS interface. The ssl module is where actual networking happens -
// ssl.wrap_socket() or SSLContext.wrap_socket() triggers the TLS connection.

/// Python code to create and inject the ssl_eryx shim module.
/// This replaces sys.modules['ssl'] with our TLS-backed implementation.
pub const SSL_SHIM_CODE: &str = r#"
import sys as _sys
import types as _types

# Create ssl_eryx module
_ssl_eryx = _types.ModuleType('ssl')
_ssl_eryx.__doc__ = 'Eryx ssl shim - minimal ssl module backed by eryx TLS imports.'

exec(compile(r'''
"""Eryx ssl shim - minimal ssl module backed by eryx TLS imports.

This module provides the ssl API that HTTP client libraries need. Actual
TLS connections are performed via the _eryx module's TLS functions which
call the WIT TLS interface.
"""

import _eryx

# Protocol constants (for compatibility checks)
PROTOCOL_TLS = 2
PROTOCOL_TLS_CLIENT = 16
PROTOCOL_TLS_SERVER = 17
PROTOCOL_SSLv23 = PROTOCOL_TLS  # Alias

# Verification modes
CERT_NONE = 0
CERT_OPTIONAL = 1
CERT_REQUIRED = 2

# Feature flags that urllib3/httpx check
HAS_SNI = True
HAS_ALPN = True
HAS_NPN = False
HAS_NEVER_CHECK_COMMON_NAME = True
HAS_SSLv2 = False
HAS_SSLv3 = False
HAS_TLSv1 = False
HAS_TLSv1_1 = False
HAS_TLSv1_2 = True
HAS_TLSv1_3 = True

# Options (ignored - host handles TLS config)
OP_NO_SSLv2 = 0x01000000
OP_NO_SSLv3 = 0x02000000
OP_NO_TLSv1 = 0x04000000
OP_NO_TLSv1_1 = 0x10000000
OP_NO_TLSv1_2 = 0x08000000
OP_NO_COMPRESSION = 0x00020000
OP_NO_TICKET = 0x00004000
OP_ALL = 0x80000FFF
OP_SINGLE_DH_USE = 0x100000
OP_SINGLE_ECDH_USE = 0x80000
OP_CIPHER_SERVER_PREFERENCE = 0x400000

# Alert descriptions
ALERT_DESCRIPTION_HANDSHAKE_FAILURE = 40
ALERT_DESCRIPTION_CERTIFICATE_UNKNOWN = 46

# Verify flags
VERIFY_DEFAULT = 0
VERIFY_CRL_CHECK_LEAF = 4
VERIFY_CRL_CHECK_CHAIN = 12
VERIFY_X509_STRICT = 32
VERIFY_X509_TRUSTED_FIRST = 64


class SSLError(OSError):
    """SSL/TLS error."""
    pass


class SSLCertVerificationError(SSLError):
    """Certificate verification failed."""
    pass


class CertificateError(SSLError):
    """Certificate error."""
    pass


class SSLWantReadError(SSLError):
    """Non-blocking operation would block on read."""
    pass


class SSLWantWriteError(SSLError):
    """Non-blocking operation would block on write."""
    pass


class SSLContext:
    """SSL context for configuring TLS connections.

    Most settings are ignored - the host controls actual TLS configuration.
    This exists for API compatibility with urllib3/httpx.
    """

    def __init__(self, protocol=PROTOCOL_TLS_CLIENT):
        self.protocol = protocol
        self.verify_mode = CERT_REQUIRED
        self.check_hostname = True
        self.options = OP_ALL | OP_NO_SSLv2 | OP_NO_SSLv3
        self._alpn_protocols = None
        self.minimum_version = None
        self.maximum_version = None
        self.hostname_checks_common_name = False
        self.post_handshake_auth = False  # Python 3.8+ attribute, ignored in sandbox

    @property
    def verify_flags(self):
        return VERIFY_DEFAULT

    @verify_flags.setter
    def verify_flags(self, value):
        pass  # Ignored

    def set_alpn_protocols(self, protocols):
        self._alpn_protocols = protocols

    def set_ciphers(self, ciphers):
        pass  # Host controls ciphers

    def set_default_verify_paths(self):
        pass  # Host uses system certs

    def load_default_certs(self, purpose=None):
        pass  # Host uses system certs

    def load_cert_chain(self, certfile, keyfile=None, password=None):
        pass  # Client certs not supported yet

    def load_verify_locations(self, cafile=None, capath=None, cadata=None):
        pass  # Host handles cert verification

    def wrap_socket(self, sock, server_side=False, do_handshake_on_connect=True,
                    suppress_ragged_eofs=True, server_hostname=None,
                    session=None):
        if server_side:
            raise SSLError("Server-side TLS not supported in sandbox")
        return SSLSocket(sock, self, server_hostname, do_handshake_on_connect)

    def wrap_bio(self, incoming, outgoing, server_side=False,
                 server_hostname=None, session=None):
        raise SSLError("wrap_bio not supported in sandbox")


class SSLSocket:
    """TLS-wrapped socket.

    Performs the actual TLS connection via _eryx TLS functions.
    """

    def __init__(self, sock, context, server_hostname, do_handshake_on_connect):
        self._sock = sock
        self._context = context
        self._server_hostname = server_hostname
        self._connected = False
        self._closed = False

        if do_handshake_on_connect and sock._tcp_handle is not None:
            self.do_handshake()

    def do_handshake(self):
        """Perform TLS handshake by upgrading the TCP connection."""
        if self._connected:
            return

        # Socket must already have a TCP connection
        if self._sock._tcp_handle is None:
            raise SSLError("Socket not connected - call connect() before wrap_socket()")

        hostname = self._server_hostname
        if not hostname and self._sock._pending_address:
            hostname = self._sock._pending_address[0]
        if not hostname:
            raise SSLError("No hostname available for TLS handshake")

        import _eryx
        result_type, value = _eryx._eryx_tls_upgrade(self._sock._tcp_handle, hostname)

        if result_type == 0:
            # Success - value is the TLS handle
            self._sock._tls_handle = value
            self._connected = True
        elif result_type == 1:
            # Error - value is the error message
            error_str = str(value)
            if 'handshake' in error_str.lower() or 'certificate' in error_str.lower():
                raise SSLCertVerificationError(error_str)
            raise SSLError(error_str)
        else:
            # Pending should never happen with ignore_wit
            raise SSLError("Unexpected pending result in sync context")

    def read(self, length=1024, buffer=None):
        if self._closed:
            raise SSLError("SSL socket is closed")
        if not self._connected:
            raise SSLError("SSL socket not connected")
        data = self._sock.recv(length)
        if buffer is not None:
            n = len(data)
            buffer[:n] = data
            return n
        return data

    def write(self, data):
        if self._closed:
            raise SSLError("SSL socket is closed")
        if not self._connected:
            raise SSLError("SSL socket not connected")
        return self._sock.send(data)

    def recv(self, bufsize, flags=0):
        return self.read(bufsize)

    def recv_into(self, buffer, nbytes=0, flags=0):
        return self.read(nbytes or len(buffer), buffer)

    def send(self, data, flags=0):
        return self.write(data)

    def sendall(self, data, flags=0):
        self._sock.sendall(data)

    def close(self):
        if not self._closed:
            self._closed = True
            self._sock.close()
            self._connected = False

    def shutdown(self, how):
        pass  # TLS shutdown happens on close

    def unwrap(self):
        """Remove the SSL layer and return the underlying socket."""
        sock = self._sock
        self._connected = False
        return sock

    def makefile(self, mode='r', buffering=-1, **kwargs):
        return self._sock.makefile(mode, buffering, **kwargs)

    def getpeercert(self, binary_form=False):
        """Return peer certificate info.

        Returns minimal info - actual cert verification happens on host.
        """
        if not self._connected:
            return None
        # Return empty dict/bytes - cert is verified by host
        return b'' if binary_form else {}

    def version(self):
        return "TLSv1.3"

    def cipher(self):
        return ("TLS_AES_256_GCM_SHA384", "TLSv1.3", 256)

    def selected_alpn_protocol(self):
        return "http/1.1"  # Could expose from host if needed

    def selected_npn_protocol(self):
        return None

    def compression(self):
        return None

    def pending(self):
        return 0

    @property
    def server_hostname(self):
        return self._server_hostname

    @property
    def context(self):
        return self._context

    @property
    def server_side(self):
        return False

    def settimeout(self, timeout):
        self._sock.settimeout(timeout)

    def gettimeout(self):
        return self._sock.gettimeout()

    def setblocking(self, flag):
        self._sock.setblocking(flag)

    def fileno(self):
        return self._sock.fileno()

    def getpeername(self):
        return self._sock.getpeername()

    def getsockname(self):
        return self._sock.getsockname()

    def dup(self):
        raise SSLError("dup() not supported in sandbox")

    def detach(self):
        return self._sock.detach()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class Purpose:
    """Purpose for certificate verification."""
    SERVER_AUTH = "SERVER_AUTH"
    CLIENT_AUTH = "CLIENT_AUTH"


class TLSVersion:
    """TLS version enumeration."""
    SSLv3 = 0x0300
    TLSv1 = 0x0301
    TLSv1_1 = 0x0302
    TLSv1_2 = 0x0303
    TLSv1_3 = 0x0304
    MINIMUM_SUPPORTED = TLSv1_2
    MAXIMUM_SUPPORTED = TLSv1_3


def create_default_context(purpose=Purpose.SERVER_AUTH, cafile=None, capath=None, cadata=None):
    """Create a default SSL context (used by urllib3/httpx)."""
    ctx = SSLContext(PROTOCOL_TLS_CLIENT)
    ctx.verify_mode = CERT_REQUIRED
    ctx.check_hostname = True
    if purpose == Purpose.SERVER_AUTH:
        ctx.verify_mode = CERT_REQUIRED
        ctx.check_hostname = True
    return ctx


def _create_unverified_context(protocol=PROTOCOL_TLS_CLIENT, cert_reqs=None,
                               check_hostname=False, purpose=None,
                               certfile=None, keyfile=None, cafile=None,
                               capath=None, cadata=None):
    """Create an unverified context (verification still happens on host)."""
    ctx = SSLContext(protocol)
    ctx.verify_mode = cert_reqs if cert_reqs is not None else CERT_NONE
    ctx.check_hostname = check_hostname
    return ctx


_create_default_https_context = create_default_context


def wrap_socket(sock, keyfile=None, certfile=None, server_side=False,
                cert_reqs=CERT_NONE, ssl_version=PROTOCOL_TLS,
                ca_certs=None, do_handshake_on_connect=True,
                suppress_ragged_eofs=True, ciphers=None,
                server_hostname=None):
    """Wrap a socket in SSL."""
    ctx = SSLContext(ssl_version)
    ctx.verify_mode = cert_reqs
    return ctx.wrap_socket(sock, server_side=server_side,
                           do_handshake_on_connect=do_handshake_on_connect,
                           server_hostname=server_hostname)


def match_hostname(cert, hostname):
    """Match hostname (deprecated but some libs still use it)."""
    pass  # Host already verified hostname


def get_server_certificate(addr, ssl_version=PROTOCOL_TLS, ca_certs=None, timeout=None):
    """Retrieve a server's certificate."""
    raise SSLError("get_server_certificate not supported in sandbox")


def DER_cert_to_PEM_cert(der_cert_bytes):
    """Convert DER to PEM format."""
    import base64
    pem = base64.standard_b64encode(der_cert_bytes).decode('ascii')
    return f"-----BEGIN CERTIFICATE-----\n{pem}\n-----END CERTIFICATE-----\n"


def PEM_cert_to_DER_cert(pem_cert_string):
    """Convert PEM to DER format."""
    import base64
    lines = pem_cert_string.strip().split('\n')
    lines = [l for l in lines if not l.startswith('-----')]
    return base64.standard_b64decode(''.join(lines))


# RAND functions (no-op, host has good entropy)
def RAND_status():
    return 1

def RAND_add(string, entropy):
    pass

def RAND_bytes(n):
    import os
    return os.urandom(n)

def RAND_pseudo_bytes(n):
    return (RAND_bytes(n), True)


# OpenSSL version info (fake but compatible)
OPENSSL_VERSION = "OpenSSL 3.0.0 (eryx TLS shim)"
OPENSSL_VERSION_INFO = (3, 0, 0, 0, 0)
OPENSSL_VERSION_NUMBER = 0x30000000

def get_default_verify_paths():
    """Return default certificate paths."""
    class DefaultVerifyPaths:
        cafile = None
        capath = None
        openssl_cafile_env = 'SSL_CERT_FILE'
        openssl_cafile = None
        openssl_capath_env = 'SSL_CERT_DIR'
        openssl_capath = None
    return DefaultVerifyPaths()

def enum_certificates(store_name):
    """Enumerate certificates (Windows only, not supported)."""
    return []

def enum_crls(store_name):
    """Enumerate CRLs (Windows only, not supported)."""
    return []

''', '<ssl_eryx>', 'exec'), _ssl_eryx.__dict__)

# Register the module
_sys.modules['ssl'] = _ssl_eryx
_sys.modules['_ssl'] = _ssl_eryx
"#;

/// Python code to create and inject the socket_eryx shim module.
/// This replaces sys.modules['socket'] with our TLS-backed implementation.
pub const SOCKET_SHIM_CODE: &str = r#"
import sys as _sys
import types as _types

# Create socket_eryx module
_socket_eryx = _types.ModuleType('socket')
_socket_eryx.__doc__ = 'Eryx socket shim - minimal socket module for HTTP client compatibility.'

exec(compile(r'''
"""Eryx socket shim - minimal socket module for HTTP client compatibility.

This module provides just enough socket API for HTTP client libraries (requests,
httpx, urllib3) to work. Actual networking is deferred to the ssl module which
uses eryx's TLS WIT imports.
"""

# Socket constants that libraries check for
AF_INET = 2
AF_INET6 = 10
AF_UNIX = 1
SOCK_STREAM = 1
SOCK_DGRAM = 2
IPPROTO_TCP = 6
IPPROTO_UDP = 17
SOL_SOCKET = 1
SO_KEEPALIVE = 9
SO_REUSEADDR = 2
TCP_NODELAY = 1
SHUT_RD = 0
SHUT_WR = 1
SHUT_RDWR = 2

# Feature flags
has_ipv6 = False
has_dualstack_ipv6 = lambda: False

# Global default timeout sentinel (used by http.client)
_GLOBAL_DEFAULT_TIMEOUT = object()


class timeout(OSError):
    """Socket timeout exception."""
    pass


class error(OSError):
    """Socket error exception."""
    pass


class herror(error):
    """Host error exception."""
    pass


class gaierror(error):
    """getaddrinfo error exception."""
    pass


class SocketIO:
    """File-like wrapper for socket (used by http.client)."""

    def __init__(self, sock, mode):
        self._sock = sock
        self._mode = mode
        self._closed = False

    def read(self, size=-1):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        if size < 0:
            # Read all available data
            chunks = []
            while True:
                chunk = self._sock.recv(8192)
                if not chunk:
                    break
                chunks.append(chunk)
            return b''.join(chunks)
        return self._sock.recv(size)

    def readinto(self, b):
        data = self.read(len(b))
        n = len(data)
        b[:n] = data
        return n

    def readline(self, limit=-1):
        # Simple line reading - HTTP headers are typically small
        result = b''
        while True:
            c = self._sock.recv(1)
            if not c:
                break
            result += c
            if c == b'\n':
                break
            if limit > 0 and len(result) >= limit:
                break
        return result

    def readlines(self, hint=-1):
        lines = []
        while True:
            line = self.readline()
            if not line:
                break
            lines.append(line)
        return lines

    def write(self, data):
        if self._closed:
            raise ValueError("I/O operation on closed file")
        return self._sock.send(data)

    def writelines(self, lines):
        for line in lines:
            self.write(line)

    def flush(self):
        pass

    def close(self):
        self._closed = True
        # Don't close underlying socket - that's the caller's responsibility

    def readable(self):
        return 'r' in self._mode or '+' in self._mode

    def writable(self):
        return 'w' in self._mode or '+' in self._mode

    def seekable(self):
        return False

    @property
    def closed(self):
        return self._closed

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class socket:
    """Minimal socket implementation for HTTP client compatibility.

    Supports both plain TCP connections (for http://) and TLS connections
    (for https:// via ssl.wrap_socket).
    """

    def __init__(self, family=AF_INET, type=SOCK_STREAM, proto=0, fileno=None):
        self._family = family
        self._type = type
        self._proto = proto
        self._timeout = None
        self._blocking = True
        self._pending_address = None
        self._tcp_handle = None   # TCP connection handle
        self._tls_handle = None   # TLS handle (set when upgraded via ssl.wrap_socket)
        self._closed = False

    @property
    def family(self):
        return self._family

    @property
    def type(self):
        return self._type

    @property
    def proto(self):
        return self._proto

    def settimeout(self, timeout):
        self._timeout = timeout
        self._blocking = timeout is None

    def gettimeout(self):
        return self._timeout

    def setblocking(self, flag):
        self._blocking = bool(flag)
        self._timeout = None if flag else 0.0

    def getblocking(self):
        return self._blocking

    def setsockopt(self, level, optname, value, optlen=None):
        pass  # Ignore socket options

    def getsockopt(self, level, optname, buflen=None):
        return 0  # Return dummy value

    def connect(self, address):
        """Connect to address over TCP."""
        if self._closed:
            raise error("Socket is closed")
        if self._tcp_handle is not None:
            raise error("Socket is already connected")

        host, port = address
        import _eryx
        result_type, value = _eryx._eryx_tcp_connect(host, port)

        if result_type == 0:
            # Success - value is the TCP handle
            self._tcp_handle = value
        elif result_type == 1:
            # Error - value is the error message
            raise error(value)
        else:
            # Pending should never happen with ignore_wit
            raise error(f"Unexpected pending result in sync context")

    def connect_ex(self, address):
        """Connect and return error code instead of raising."""
        try:
            self.connect(address)
            return 0
        except error:
            return 1

    def bind(self, address):
        raise error("bind() not supported in sandbox")

    def listen(self, backlog=None):
        raise error("listen() not supported in sandbox")

    def accept(self):
        raise error("accept() not supported in sandbox")

    def close(self):
        if self._closed:
            return
        self._closed = True
        # Note: We intentionally do NOT close the handles here immediately.
        # This is because http.client and other libraries may call close() on
        # the socket while still expecting to read data through a makefile() wrapper.
        # The handles will be closed when the socket is garbage collected via __del__.

    def _force_close(self):
        """Actually close the underlying handles. Called by __del__."""
        import _eryx
        if self._tls_handle is not None:
            try:
                _eryx._eryx_tls_close(self._tls_handle)
            except Exception:
                pass
            self._tls_handle = None
        if self._tcp_handle is not None:
            try:
                _eryx._eryx_tcp_close(self._tcp_handle)
            except Exception:
                pass
            self._tcp_handle = None

    def __del__(self):
        self._force_close()

    def shutdown(self, how):
        pass  # Shutdown happens on close

    def detach(self):
        tcp_handle = self._tcp_handle
        self._tcp_handle = None
        self._tls_handle = None
        return -1  # No real fd

    def recv(self, bufsize, flags=0):
        """Receive data from the socket."""
        # Use TLS handle if upgraded, otherwise TCP handle
        handle = self._tls_handle if self._tls_handle is not None else self._tcp_handle

        # If no handle available, return empty bytes (EOF)
        # Note: We allow reads even after close() because libraries like http.client
        # may close the socket but still read through a makefile() wrapper.
        # The handles are only closed in __del__, not in close().
        if handle is None:
            return b""

        import _eryx
        if self._tls_handle is not None:
            result_type, value = _eryx._eryx_tls_read(handle, bufsize)
        else:
            result_type, value = _eryx._eryx_tcp_read(handle, bufsize)

        if result_type == 0:
            # Success - value is the bytes read
            return bytes(value) if value else b""
        elif result_type == 1:
            # Error - value is the error message
            raise error(value)
        else:
            # Pending should never happen with ignore_wit
            raise error(f"Unexpected pending result in sync context")

    def recv_into(self, buffer, nbytes=0, flags=0):
        """Receive data into a buffer."""
        length = nbytes if nbytes > 0 else len(buffer)
        data = self.recv(length)
        n = len(data)
        buffer[:n] = data
        return n

    def send(self, data, flags=0):
        """Send data to the socket."""
        if self._closed:
            raise error("Socket is closed")

        # Use TLS handle if upgraded, otherwise TCP handle
        handle = self._tls_handle if self._tls_handle is not None else self._tcp_handle
        if handle is None:
            raise error("Socket is not connected")

        if isinstance(data, memoryview):
            data = bytes(data)

        import _eryx
        if self._tls_handle is not None:
            result_type, value = _eryx._eryx_tls_write(handle, data)
        else:
            result_type, value = _eryx._eryx_tcp_write(handle, data)

        if result_type == 0:
            # Success - value is the number of bytes written
            return value
        elif result_type == 1:
            # Error - value is the error message
            raise error(value)
        else:
            # Pending should never happen with ignore_wit
            raise error(f"Unexpected pending result in sync context")

    def sendall(self, data, flags=0):
        if isinstance(data, memoryview):
            data = bytes(data)
        sent = 0
        while sent < len(data):
            n = self.send(data[sent:], flags)
            if n == 0:
                raise error("Connection closed")
            sent += n

    def makefile(self, mode='r', buffering=-1, **kwargs):
        """Return a file-like object for the socket."""
        return SocketIO(self, mode)

    def getpeername(self):
        if self._pending_address:
            return self._pending_address
        raise error("Socket not connected")

    def getsockname(self):
        return ('0.0.0.0', 0)

    def fileno(self):
        return -1  # No real file descriptor

    def dup(self):
        raise error("dup() not supported in sandbox")

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def __repr__(self):
        return f"<socket.socket fd={self.fileno()}, family={self._family}, type={self._type}>"


def create_connection(address, timeout=None, source_address=None, *, all_errors=False):
    """Create a connected socket (used by urllib3)."""
    host, port = address
    sock = socket(AF_INET, SOCK_STREAM)
    if timeout is not None:
        sock.settimeout(timeout)
    sock.connect((host, port))
    return sock


def getaddrinfo(host, port, family=0, type=0, proto=0, flags=0):
    """Minimal getaddrinfo for HTTP clients.

    Returns fake results - actual DNS happens on host during TLS connect.
    """
    # Normalize port to int
    if isinstance(port, str):
        port = int(port) if port else 0
    elif port is None:
        port = 0

    # Return IPv4 TCP result - this is what HTTP clients need
    return [(AF_INET, SOCK_STREAM, IPPROTO_TCP, '', (str(host), port))]


def gethostbyname(hostname):
    """Return hostname as-is - DNS happens on host."""
    return str(hostname)


def gethostbyname_ex(hostname):
    """Extended gethostbyname - return hostname as-is."""
    return (str(hostname), [], [str(hostname)])


def gethostbyaddr(ip_address):
    """Reverse DNS lookup - not supported."""
    return (str(ip_address), [], [str(ip_address)])


def getfqdn(name=''):
    """Get fully qualified domain name."""
    return name if name else 'localhost'


def gethostname():
    """Get local hostname."""
    return 'sandbox'


def getservbyname(servicename, protocolname=None):
    """Get port number for service name."""
    services = {'http': 80, 'https': 443, 'ftp': 21, 'ssh': 22}
    return services.get(servicename.lower(), 0)


def getservbyport(port, protocolname=None):
    """Get service name for port."""
    services = {80: 'http', 443: 'https', 21: 'ftp', 22: 'ssh'}
    return services.get(port, str(port))


def getprotobyname(protocolname):
    """Get protocol number by name."""
    protocols = {'tcp': IPPROTO_TCP, 'udp': IPPROTO_UDP}
    return protocols.get(protocolname.lower(), 0)


def getdefaulttimeout():
    """Get default socket timeout."""
    return None


def setdefaulttimeout(timeout):
    """Set default socket timeout (ignored)."""
    pass


def socketpair(family=AF_UNIX, type=SOCK_STREAM, proto=0):
    """Create a pair of connected sockets (dummy for asyncio)."""
    # Return dummy sockets for asyncio's self-pipe trick
    class _DummySocket:
        def __init__(self):
            self._buffer = []
            self._closed = False
        def fileno(self):
            return -1
        def setblocking(self, flag):
            pass
        def send(self, data):
            self._buffer.append(data)
            return len(data)
        def recv(self, n):
            if self._buffer:
                return self._buffer.pop(0)
            return b''
        def close(self):
            self._closed = True
        def __enter__(self):
            return self
        def __exit__(self, *args):
            self.close()
    return (_DummySocket(), _DummySocket())


def inet_aton(ip_string):
    """Convert IPv4 address to packed binary."""
    parts = ip_string.split('.')
    return bytes(int(p) for p in parts)


def inet_ntoa(packed_ip):
    """Convert packed binary to IPv4 address string."""
    return '.'.join(str(b) for b in packed_ip)


def inet_pton(address_family, ip_string):
    """Convert IP address to packed binary."""
    if address_family == AF_INET:
        return inet_aton(ip_string)
    raise error(f"Address family {address_family} not supported")


def inet_ntop(address_family, packed_ip):
    """Convert packed binary to IP address string."""
    if address_family == AF_INET:
        return inet_ntoa(packed_ip)
    raise error(f"Address family {address_family} not supported")


def ntohs(x):
    """Network to host short."""
    return ((x & 0xff) << 8) | ((x >> 8) & 0xff)


def ntohl(x):
    """Network to host long."""
    return (((x & 0xff) << 24) | ((x & 0xff00) << 8) |
            ((x >> 8) & 0xff00) | ((x >> 24) & 0xff))


def htons(x):
    """Host to network short."""
    return ntohs(x)


def htonl(x):
    """Host to network long."""
    return ntohl(x)

''', '<socket_eryx>', 'exec'), _socket_eryx.__dict__)

# Register the module
_sys.modules['socket'] = _socket_eryx
_sys.modules['_socket'] = _socket_eryx
"#;

pub fn initialize_python() {
    if PYTHON_INITIALIZED.swap(true, Ordering::SeqCst) {
        // Already initialized
        return;
    }

    // Register the PyO3-generated _eryx module BEFORE Py_Initialize.
    // This is how componentize-py does it - using the pyo3 macro.
    pyo3::append_to_inittab!(eryx_module);

    unsafe {
        // Initialize Python without registering signal handlers.
        // Signal handlers don't make sense in a WASM sandbox.
        // Note: PYTHONPATH must be set before this call for Python to find stdlib.
        Py_InitializeEx(0);

        // Ensure __main__ module exists for code execution.
        // sys.path is already configured via PYTHONPATH environment variable.
        let setup_code = c"import __main__";
        let result = PyRun_SimpleString(setup_code.as_ptr());
        if result != 0 {
            // If this fails, Python is in a bad state - not much we can do
            PyErr_Clear();
        }

        // Inject the _eryx_async module into sys.modules.
        // This provides our embedded async runtime without needing external files.
        let inject_cstr = std::ffi::CString::new(ERYX_ASYNC_INJECT_CODE).unwrap();
        let result = PyRun_SimpleString(inject_cstr.as_ptr());
        if result != 0 {
            // _eryx_async injection failed - this is critical for async support
            // Log the error but continue (sync code will still work)
            PyErr_Clear();
        }

        // Set up execution infrastructure (stdout capture, _eryx_exec, etc.)
        // This is done ONCE here, not on every execute() call.
        let infra_cstr = std::ffi::CString::new(ERYX_EXEC_INFRASTRUCTURE).unwrap();
        let result = PyRun_SimpleString(infra_cstr.as_ptr());
        if result != 0 {
            // Infrastructure setup failed - this is critical
            PyErr_Clear();
        }

        // Inject the socket shim module.
        // This replaces sys.modules['socket'] with our TLS-backed implementation.
        let socket_cstr = std::ffi::CString::new(SOCKET_SHIM_CODE).unwrap();
        let result = PyRun_SimpleString(socket_cstr.as_ptr());
        if result != 0 {
            // Socket shim injection failed - networking won't work
            // Note: Can't use tracing here - this runs in WASM context
            PyErr_Clear();
        }

        // Inject the ssl shim module.
        // This replaces sys.modules['ssl'] with our TLS-backed implementation.
        let ssl_cstr = std::ffi::CString::new(SSL_SHIM_CODE).unwrap();
        let result = PyRun_SimpleString(ssl_cstr.as_ptr());
        if result != 0 {
            // SSL shim injection failed - TLS won't work
            // Note: Can't use tracing here - this runs in WASM context
            PyErr_Clear();
        }

        // Note: We do NOT call reset_wasi_state() here!
        //
        // The reset must happen AFTER all imports are done, not here during
        // Python initialization. If we reset here, any file handles opened
        // during `execute("import numpy")` etc. would still get captured.
        //
        // Instead, the host calls `finalize-preinit` export after imports
        // are complete, which calls `finalize_preinit()` -> `reset_wasi_state()`.
    }
}

/// Finalize pre-initialization by resetting WASI state.
///
/// This MUST be called at the end of pre-initialization, after all imports
/// are done but before the memory snapshot is captured. It clears file handles
/// from the WASI adapter and wasi-libc so they don't get captured in the
/// snapshot (which would cause "unknown handle index" errors at runtime).
///
/// This is only meant to be called during component-init-transform
/// pre-initialization. Calling it at runtime has no useful effect.
pub fn finalize_preinit() {
    reset_wasi_state();
}

/// Reset WASI adapter and wasi-libc state.
///
/// This clears any file handles that were opened during initialization,
/// which is necessary for pre-initialization to work correctly.
///
/// - `reset_adapter_state`: Tells the WASI Preview 1 adapter to forget open handles
/// - `__wasilibc_reset_preopens`: Tells wasi-libc to forget preopen state
fn reset_wasi_state() {
    // Import reset_adapter_state from the WASI adapter.
    // This tells the WASI Preview 1 adapter to forget about any open handles.
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        #[link_name = "reset_adapter_state"]
        fn reset_adapter_state();
    }

    // __wasilibc_reset_preopens is in wasi-libc. When compiling as a dynamic library,
    // it becomes an import from "env" which is resolved to libc.so during component linking.
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn __wasilibc_reset_preopens();
    }

    unsafe {
        reset_adapter_state();
        __wasilibc_reset_preopens();
    }
}

/// Check if Python is initialized.
pub fn is_python_initialized() -> bool {
    PYTHON_INITIALIZED.load(Ordering::SeqCst)
}

// =============================================================================
// Safe wrappers for common operations
// =============================================================================

/// Run simple Python code in __main__.
///
/// Returns Ok(()) on success, Err with error message on failure.
///
/// # Safety
///
/// Python must be initialized before calling this.
pub unsafe fn run_simple_string(code: &str) -> Result<(), String> {
    use std::ffi::CString;

    let code_cstr = CString::new(code).map_err(|e| format!("Invalid code string: {e}"))?;

    let result = unsafe { PyRun_SimpleString(code_cstr.as_ptr()) };

    if result == 0 {
        Ok(())
    } else {
        // An exception occurred - try to get error info
        let err_msg = unsafe { get_last_error_message() };
        Err(err_msg)
    }
}

/// Get the last Python error message.
///
/// Clears the error indicator.
///
/// # Safety
///
/// Python must be initialized.
pub unsafe fn get_last_error_message() -> String {
    unsafe {
        if PyErr_Occurred().is_null() {
            return "Unknown error".to_string();
        }

        // Get the current exception (clears PyErr state)
        let exc = PyErr_GetRaisedException();

        if exc.is_null() {
            return "Unknown error (no exception)".to_string();
        }

        // Try to get string representation of the exception
        let str_obj = PyObject_Str(exc);
        let result = if str_obj.is_null() {
            "Error converting exception to string".to_string()
        } else {
            let utf8 = PyUnicode_AsUTF8(str_obj);
            let msg = if utf8.is_null() {
                "Error getting UTF-8 from exception".to_string()
            } else {
                std::ffi::CStr::from_ptr(utf8)
                    .to_string_lossy()
                    .into_owned()
            };
            Py_DecRef(str_obj);
            msg
        };

        Py_DecRef(exc);

        result
    }
}

/// Get a Python variable's string value from __main__.
///
/// # Safety
///
/// Python must be initialized.
pub unsafe fn get_python_variable_string(name: &str) -> Result<String, String> {
    use std::ffi::CString;

    let name_cstr = CString::new(name).map_err(|e| format!("Invalid variable name: {e}"))?;
    let main_cstr = CString::new("__main__").unwrap();

    unsafe {
        // Get __main__ module
        let main_module = PyImport_AddModule(main_cstr.as_ptr());
        if main_module.is_null() {
            return Err("Failed to get __main__ module".to_string());
        }

        // Get __main__.__dict__
        let main_dict = PyModule_GetDict(main_module);
        if main_dict.is_null() {
            return Err("Failed to get __main__.__dict__".to_string());
        }

        // Get the variable
        let var = PyDict_GetItemString(main_dict, name_cstr.as_ptr());
        if var.is_null() {
            return Err(format!("Variable '{name}' not found"));
        }

        // Convert to string
        let str_obj = PyObject_Str(var);
        if str_obj.is_null() {
            let err = get_last_error_message();
            return Err(format!("Failed to convert '{name}' to string: {err}"));
        }

        let utf8 = PyUnicode_AsUTF8(str_obj);
        let result = if utf8.is_null() {
            Py_DecRef(str_obj);
            return Err("Failed to get UTF-8 from string".to_string());
        } else {
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned()
        };

        Py_DecRef(str_obj);
        Ok(result)
    }
}

// =============================================================================
// Execute Python code with output capture
// =============================================================================

/// Escape a string for use as a Python string literal (triple-quoted).
fn python_string_literal(s: &str) -> String {
    // Use triple double-quotes. We need to escape any triple-quote sequences
    // and trailing backslashes (which would escape the closing quotes).
    // Don't use raw strings so that escape sequences like \n work.
    let mut escaped = s.replace('\\', "\\\\").replace("\"\"\"", r#"\"\"\""#);
    // A trailing backslash would escape the closing quotes, so add a space and strip it later
    // Actually, the double-escaping handles this - a trailing \\ becomes \\\\
    // But we need to handle the case where user has """
    escaped = escaped
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"\"\"{}\"\"\"", escaped)
}

/// Execute Python code and capture stdout.
///
/// This is the main entry point for the `execute` WIT export.
/// It runs the provided code in `__main__` and returns captured stdout,
/// or an error message if execution fails.
///
/// Supports top-level `await` by detecting async code and wrapping it
/// in an async function executed via `asyncio.run()`.
///
/// # Returns
/// - `Complete(stdout)` - The captured stdout output (may be empty)
/// - `Error(message)` - Error message if execution failed
/// - `Pending(waitable_set)` - Execution suspended, waiting for async callback
pub fn execute_python(code: &str) -> ExecuteResult {
    use std::ffi::CString;

    if !is_python_initialized() {
        return ExecuteResult::Error("Python not initialized".to_string());
    }

    // Validate that code is valid UTF-8 (CString requires this)
    if CString::new(code).is_err() {
        return ExecuteResult::Error("Invalid code string: contains null bytes".to_string());
    }

    unsafe {
        // Call the pre-compiled _eryx_exec() function with the user code.
        // This is MUCH faster than the old approach because:
        // 1. All infrastructure (imports, trace func, etc.) is already set up
        // 2. We only parse/compile the user code, not 100+ lines of wrapper
        // 3. stdout/stderr capture reuses existing StringIO objects
        let exec_call = format!("_eryx_exec({})", python_string_literal(code));

        let exec_code_cstr = match CString::new(exec_call) {
            Ok(s) => s,
            Err(e) => return ExecuteResult::Error(format!("Invalid exec code: {e}")),
        };
        let exec_result = PyRun_SimpleString(exec_code_cstr.as_ptr());

        if exec_result != 0 {
            // Execution failed - get error and output
            let _ = PyRun_SimpleString(c"_eryx_output, _eryx_errors = _eryx_get_output()".as_ptr());

            let stderr_output = get_python_variable_string("_eryx_errors").unwrap_or_default();
            let exception_msg = get_last_error_message();

            let error = if !stderr_output.is_empty() && exception_msg != "Unknown error" {
                format!("{stderr_output}\n{exception_msg}")
            } else if !stderr_output.is_empty() {
                stderr_output
            } else {
                exception_msg
            };

            return ExecuteResult::Error(error);
        }

        // Check the callback code to see if execution is pending
        let callback_code_value = get_python_callback_code();
        let code = callback_code::get_code(callback_code_value);

        if code == callback_code::WAIT {
            // Execution is pending - keep stdout/stderr redirected for async output
            return ExecuteResult::Pending(callback_code_value);
        }

        // Execution complete - get output and restore streams
        let _ = PyRun_SimpleString(c"_eryx_output, _eryx_errors = _eryx_get_output()".as_ptr());

        let stdout = get_python_variable_string("_eryx_output").unwrap_or_default();
        let stderr = get_python_variable_string("_eryx_errors").unwrap_or_default();

        ExecuteResult::Complete(ExecuteOutput {
            stdout: stdout.trim_end_matches('\n').to_string(),
            stderr: stderr.trim_end_matches('\n').to_string(),
        })
    }
}

// =============================================================================
// State management functions
// =============================================================================

/// Get a Python variable's bytes value from __main__.
///
/// # Safety
///
/// Python must be initialized.
unsafe fn get_python_variable_bytes(name: &str) -> Result<Vec<u8>, String> {
    use std::ffi::CString;

    let name_cstr = CString::new(name).map_err(|e| format!("Invalid variable name: {e}"))?;
    let main_cstr = CString::new("__main__").unwrap();

    unsafe {
        // Get __main__ module
        let main_module = PyImport_AddModule(main_cstr.as_ptr());
        if main_module.is_null() {
            return Err("Failed to get __main__ module".to_string());
        }

        // Get __main__.__dict__
        let main_dict = PyModule_GetDict(main_module);
        if main_dict.is_null() {
            return Err("Failed to get __main__.__dict__".to_string());
        }

        // Get the variable
        let var = PyDict_GetItemString(main_dict, name_cstr.as_ptr());
        if var.is_null() {
            return Err(format!("Variable '{name}' not found"));
        }

        // Get bytes from the bytes object
        let ptr = PyBytes_AsString(var);
        if ptr.is_null() {
            let err = get_last_error_message();
            return Err(format!("Failed to get bytes from '{name}': {err}"));
        }

        let size = PyBytes_Size(var);
        if size < 0 {
            return Err("Failed to get bytes size".to_string());
        }

        // Copy bytes to a Vec
        let slice = std::slice::from_raw_parts(ptr as *const u8, size as usize);
        Ok(slice.to_vec())
    }
}

/// Set a Python bytes variable in __main__.
///
/// # Safety
///
/// Python must be initialized.
unsafe fn set_python_variable_bytes(name: &str, data: &[u8]) -> Result<(), String> {
    use std::ffi::CString;

    let name_cstr = CString::new(name).map_err(|e| format!("Invalid variable name: {e}"))?;
    let main_cstr = CString::new("__main__").unwrap();

    unsafe {
        // Get __main__ module
        let main_module = PyImport_AddModule(main_cstr.as_ptr());
        if main_module.is_null() {
            return Err("Failed to get __main__ module".to_string());
        }

        // Get __main__.__dict__
        let main_dict = PyModule_GetDict(main_module);
        if main_dict.is_null() {
            return Err("Failed to get __main__.__dict__".to_string());
        }

        // Create a bytes object
        let bytes_obj = PyBytes_FromStringAndSize(data.as_ptr() as *const i8, data.len() as isize);
        if bytes_obj.is_null() {
            let err = get_last_error_message();
            return Err(format!("Failed to create bytes object: {err}"));
        }

        // Set the variable in __main__
        let result = PyDict_SetItemString(main_dict, name_cstr.as_ptr(), bytes_obj);
        Py_DecRef(bytes_obj); // SetItem increments ref, so we decrement ours

        if result != 0 {
            let err = get_last_error_message();
            return Err(format!("Failed to set variable '{name}': {err}"));
        }

        Ok(())
    }
}

/// Snapshot the current Python state by pickling `_eryx_user_globals`.
///
/// Returns the pickled state as bytes, which can be restored later with `restore_state`.
///
/// # What is preserved
/// - All user-defined variables in the user namespace
/// - Simple types (int, float, str, list, dict, tuple, set, etc.)
/// - Most standard library objects
///
/// # What is NOT preserved
/// - Open file handles, sockets, etc.
/// - Imported modules (they remain, but aren't pickled)
/// - Objects with unpicklable state
pub fn snapshot_state() -> Result<Vec<u8>, String> {
    if !is_python_initialized() {
        return Err("Python not initialized".to_string());
    }

    unsafe {
        // Pickle _eryx_user_globals, excluding unpicklable items
        let pickle_code = c"
import pickle as _eryx_pickle

# Items to exclude from snapshot (builtins and metadata)
_eryx_exclude = {
    '__builtins__', '__name__', '__doc__', '__package__',
    '__loader__', '__spec__', '__cached__', '__file__',
    # Exclude callback infrastructure (set up fresh on each run)
    'invoke', 'list_callbacks', '_EryxNamespace', '_EryxCallbackLeaf',
    '_eryx_make_callback', '_eryx_reserved',
}

# Check if an object is part of the callback infrastructure
def _eryx_is_callback_obj(obj):
    # Check for callback wrapper functions (created by _eryx_make_callback)
    if callable(obj) and hasattr(obj, '__closure__') and obj.__closure__:
        for cell in obj.__closure__:
            try:
                if 'invoke' in _eryx_user_globals and cell.cell_contents == _eryx_user_globals['invoke']:
                    return True
            except (ValueError, NameError):
                pass
    # Check for namespace objects
    obj_type = type(obj).__name__
    if obj_type in ('_EryxNamespace', '_EryxCallbackLeaf'):
        return True
    return False

# Take a snapshot of the keys first to avoid 'dictionary changed size during iteration'
_eryx_keys = list(_eryx_user_globals.keys())

# Build dict of picklable items
_eryx_state_dict = {}
for _k in _eryx_keys:
    if _k not in _eryx_exclude and not _k.startswith('_eryx_'):
        _v = _eryx_user_globals.get(_k)
        if _v is not None:
            # Skip callback infrastructure objects
            if _eryx_is_callback_obj(_v):
                continue
            try:
                # Test if item is picklable
                _eryx_pickle.dumps(_v)
                _eryx_state_dict[_k] = _v
            except (TypeError, _eryx_pickle.PicklingError, AttributeError):
                # Skip unpicklable items (modules, functions with closures, etc.)
                pass

# Pickle the filtered dict
_eryx_state_bytes = _eryx_pickle.dumps(_eryx_state_dict)

# Clean up local helpers
del _eryx_exclude, _eryx_is_callback_obj, _eryx_keys, _eryx_state_dict, _eryx_pickle
";

        if PyRun_SimpleString(pickle_code.as_ptr()) != 0 {
            let err = get_last_error_message();
            PyErr_Clear();
            return Err(format!("Failed to snapshot state: {err}"));
        }

        // Get the pickled bytes
        let state_bytes = get_python_variable_bytes("_eryx_state_bytes")?;

        // Clean up
        let _ = PyRun_SimpleString(c"del _eryx_state_bytes".as_ptr());

        Ok(state_bytes)
    }
}

/// Restore Python state from a previous snapshot.
///
/// This unpickles the data and updates `_eryx_user_globals` with the restored values.
/// Existing variables that aren't in the snapshot are preserved.
pub fn restore_state(data: &[u8]) -> Result<(), String> {
    if !is_python_initialized() {
        return Err("Python not initialized".to_string());
    }

    if data.is_empty() {
        // Empty snapshot = nothing to restore
        return Ok(());
    }

    unsafe {
        // Set the bytes in Python
        set_python_variable_bytes("_eryx_restore_bytes", data)?;

        // Unpickle and update _eryx_user_globals
        let restore_code = c"
import pickle as _eryx_pickle

# Unpickle the state
_eryx_restored_dict = _eryx_pickle.loads(_eryx_restore_bytes)

# Update user globals with restored values
_eryx_user_globals.update(_eryx_restored_dict)

# Clean up
del _eryx_restore_bytes, _eryx_restored_dict, _eryx_pickle
";

        if PyRun_SimpleString(restore_code.as_ptr()) != 0 {
            let err = get_last_error_message();
            let _ = PyRun_SimpleString(c"del _eryx_restore_bytes".as_ptr());
            return Err(format!("Failed to restore state: {err}"));
        }

        Ok(())
    }
}

/// Clear all user-defined state from `_eryx_user_globals`.
///
/// This removes all variables except Python builtins and module metadata,
/// effectively resetting to a fresh interpreter state.
pub fn clear_state() {
    if !is_python_initialized() {
        return;
    }

    unsafe {
        let clear_code = c"
# Items to keep (builtins and metadata)
_eryx_keep = {
    '__builtins__', '__name__', '__doc__', '__package__',
    '__loader__', '__spec__', '__cached__', '__file__',
    # Preserve callback infrastructure
    'invoke', 'list_callbacks', '_EryxNamespace', '_EryxCallbackLeaf',
    '_eryx_make_callback', '_eryx_reserved', '_eryx_callbacks',
}

# Also keep callback wrappers and namespace objects
def _eryx_should_keep(k, v):
    if k in _eryx_keep:
        return True
    # Keep callback wrapper functions
    if callable(v) and hasattr(v, '__closure__') and v.__closure__:
        for cell in v.__closure__:
            try:
                if 'invoke' in _eryx_user_globals and cell.cell_contents == _eryx_user_globals['invoke']:
                    return True
            except (ValueError, NameError):
                pass
    # Keep namespace objects
    if type(v).__name__ in ('_EryxNamespace', '_EryxCallbackLeaf'):
        return True
    return False

# Collect keys to delete (can't modify dict during iteration)
_eryx_to_delete = [k for k, v in list(_eryx_user_globals.items()) if not _eryx_should_keep(k, v)]

# Delete the keys
for _k in _eryx_to_delete:
    del _eryx_user_globals[_k]

# Clean up our temporaries
del _eryx_keep, _eryx_should_keep, _eryx_to_delete, _k
";

        if PyRun_SimpleString(clear_code.as_ptr()) != 0 {
            // Best effort - clear errors and continue
            PyErr_Clear();
        }
    }
}

// =============================================================================
// Callback support
// =============================================================================

/// Information about a callback available from the host.
#[derive(Debug, Clone)]
pub struct CallbackInfo {
    pub name: String,
    pub description: String,
    pub parameters_schema_json: String,
}

/// Set up callback wrapper functions in Python.
///
/// This injects:
/// 1. An `invoke(name, **kwargs)` function that calls host callbacks via `_eryx`
/// 2. A `list_callbacks()` function for introspection
/// 3. Direct wrapper functions for each callback (e.g., `sleep(ms=100)`)
/// 4. Namespace objects for dotted callbacks (e.g., `http.get(url="...")`)
pub fn setup_callbacks(callbacks: &[CallbackInfo]) -> Result<(), String> {
    if !is_python_initialized() {
        return Err("Python not initialized".to_string());
    }

    unsafe {
        // Serialize callbacks to JSON for Python to parse
        let callbacks_json = serde_json_mini_serialize_callbacks(callbacks);

        // Inject the callback setup code
        let setup_code = format!(
            r#"
import json as _json
import _eryx
import _eryx_async

# Callbacks metadata from host
_eryx_callbacks_json = '''{}'''
_eryx_callbacks = _json.loads(_eryx_callbacks_json)

async def invoke(_callback_name, **kwargs):
    """Invoke a host callback by name with keyword arguments (async).

    Args:
        _callback_name: Name of the callback (e.g., "sleep", "http.get")
        **kwargs: Arguments to pass to the callback

    Returns:
        The callback result (parsed from JSON)

    Example:
        result = await invoke("get_time")
        data = await invoke("http.get", url="https://example.com")
    """
    # Serialize kwargs to JSON
    args_json = _json.dumps(kwargs)

    # Report callback start trace event
    _eryx._eryx_report_trace(0, _json.dumps({{"type": "callback_start", "name": _callback_name}}), args_json)

    try:
        # Use _eryx_async.await_invoke for proper async handling
        result_json = await _eryx_async.await_invoke(_callback_name, args_json)
        # Report callback end trace event
        _eryx._eryx_report_trace(0, _json.dumps({{"type": "callback_end", "name": _callback_name}}), "")
        if result_json:
            return _json.loads(result_json)
        return None
    except Exception as e:
        # Report callback error trace event
        _eryx._eryx_report_trace(0, _json.dumps({{"type": "callback_end", "name": _callback_name, "error": str(e)}}), "")
        raise

def list_callbacks():
    """List all available callbacks for introspection.

    Returns:
        List of callback info dicts with 'name', 'description',
        and 'parameters_schema' keys.
    """
    return [
        {{
            'name': cb['name'],
            'description': cb['description'],
            'parameters_schema': _json.loads(cb['parameters_schema_json']) if cb['parameters_schema_json'] else None
        }}
        for cb in _eryx_callbacks
    ]

# Reserved names that shouldn't be shadowed by callbacks
_eryx_reserved = set(dir(__builtins__)) | {{
    'invoke', 'list_callbacks', 'asyncio', 'json', 'math', 're',
    'os', 'subprocess', 'socket', '__import__'
}}

# Helper to create async callback wrappers
def _eryx_make_callback(_cb_name):
    async def callback(**kwargs):
        # invoke() is now async, so await it
        return await invoke(_cb_name, **kwargs)
    callback.__name__ = _cb_name
    callback.__doc__ = f"Invoke the '{{_cb_name}}' callback (async)."
    return callback

# Namespace class for dotted callbacks like http.get
class _EryxNamespace:
    def __init__(self, invoke_fn, prefix=''):
        self._invoke = invoke_fn
        self._prefix = prefix
        self._children = {{}}

    def _add_callback(self, parts):
        if len(parts) == 1:
            pass  # Leaf - handled by __getattr__
        else:
            child = parts[0]
            if child not in self._children:
                new_prefix = f"{{self._prefix}}{{child}}." if self._prefix else f"{{child}}."
                self._children[child] = _EryxNamespace(self._invoke, new_prefix)
            self._children[child]._add_callback(parts[1:])

    def __getattr__(self, name):
        if name.startswith('_'):
            raise AttributeError(name)
        if name in self._children:
            return self._children[name]
        full_name = f"{{self._prefix}}{{name}}"
        return _EryxCallbackLeaf(self._invoke, full_name)

    async def __call__(self, **kwargs):
        if self._prefix:
            return await self._invoke(self._prefix.rstrip('.'), **kwargs)
        raise TypeError("Cannot call root namespace")

class _EryxCallbackLeaf:
    def __init__(self, invoke_fn, name):
        self._invoke = invoke_fn
        self._name = name

    async def __call__(self, **kwargs):
        return await self._invoke(self._name, **kwargs)

# Generate callback wrappers - add to both module globals and user globals
_eryx_namespaces = {{}}
for _cb in _eryx_callbacks:
    _name = _cb['name']
    if '.' in _name:
        _parts = _name.split('.')
        _root = _parts[0]
        if _root not in _eryx_reserved:
            if _root not in _eryx_namespaces:
                # Create root namespace with prefix=root+'.' so children get full path
                _eryx_namespaces[_root] = _EryxNamespace(invoke, _root + '.')
            # Skip the root part since we've already accounted for it in the prefix
            _eryx_namespaces[_root]._add_callback(_parts[1:])
    else:
        if _name not in _eryx_reserved:
            _wrapper = _eryx_make_callback(_name)
            globals()[_name] = _wrapper
            _eryx_user_globals[_name] = _wrapper

# Add namespaces to both module globals and user globals
globals().update(_eryx_namespaces)
_eryx_user_globals.update(_eryx_namespaces)

# Inject core API into user globals so user code can access callbacks
_eryx_user_globals['invoke'] = invoke
_eryx_user_globals['list_callbacks'] = list_callbacks
_eryx_user_globals['_EryxNamespace'] = _EryxNamespace
_eryx_user_globals['_EryxCallbackLeaf'] = _EryxCallbackLeaf

# Clean up temporary variables
del _eryx_callbacks_json, _eryx_namespaces
try:
    del _cb, _name, _parts, _root
except NameError:
    pass
"#,
            callbacks_json.replace('\\', "\\\\").replace('\'', "\\'")
        );

        // Run the setup code
        let setup_cstr =
            std::ffi::CString::new(setup_code).map_err(|e| format!("Invalid setup code: {e}"))?;
        if PyRun_SimpleString(setup_cstr.as_ptr()) != 0 {
            let err = get_last_error_message();
            return Err(format!("Failed to set up callbacks: {err}"));
        }

        Ok(())
    }
}

/// Simple JSON serialization for callbacks (avoiding serde dependency in WASM)
fn serde_json_mini_serialize_callbacks(callbacks: &[CallbackInfo]) -> String {
    let items: Vec<String> = callbacks
        .iter()
        .map(|cb| {
            format!(
                r#"{{"name": "{}", "description": "{}", "parameters_schema_json": "{}"}}"#,
                escape_json_string(&cb.name),
                escape_json_string(&cb.description),
                escape_json_string(&cb.parameters_schema_json)
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// Escape a string for JSON
pub fn escape_json_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::escape_json_string;

    #[test]
    fn test_escape_json_string_basic() {
        assert_eq!(escape_json_string("hello"), "hello");
        assert_eq!(escape_json_string(""), "");
        assert_eq!(escape_json_string(" "), " ");
    }

    #[test]
    fn test_escape_json_string_quotes() {
        assert_eq!(escape_json_string(r#"say "hello""#), r#"say \"hello\""#);
        // Three quotes -> three escaped quotes
        assert_eq!(escape_json_string("\"\"\""), r#"\"\"\""#);
        // Single quotes don't need escaping in JSON
        assert_eq!(escape_json_string("it's"), "it's");
    }

    #[test]
    fn test_escape_json_string_backslash() {
        assert_eq!(escape_json_string(r"path\to\file"), r"path\\to\\file");
        assert_eq!(escape_json_string(r"\\"), r"\\\\");
        assert_eq!(escape_json_string(r"\"), r"\\");
    }

    #[test]
    fn test_escape_json_string_control_chars() {
        assert_eq!(escape_json_string("line1\nline2"), r"line1\nline2");
        assert_eq!(escape_json_string("col1\tcol2"), r"col1\tcol2");
        assert_eq!(escape_json_string("text\r\n"), r"text\r\n");
        // Multiple in a row
        assert_eq!(escape_json_string("\n\n\n"), r"\n\n\n");
        assert_eq!(escape_json_string("\t\t"), r"\t\t");
    }

    #[test]
    fn test_escape_json_string_all_control_chars() {
        // Null byte and other control characters should be \uXXXX escaped
        assert_eq!(escape_json_string("a\x00b"), r"a\u0000b");
        assert_eq!(escape_json_string("\x1f"), r"\u001f");
        // Test all ASCII control characters (0x00-0x1F except \t, \n, \r)
        assert_eq!(escape_json_string("\x01"), r"\u0001");
        assert_eq!(escape_json_string("\x02"), r"\u0002");
        assert_eq!(escape_json_string("\x07"), r"\u0007"); // bell
        assert_eq!(escape_json_string("\x08"), r"\u0008"); // backspace
        assert_eq!(escape_json_string("\x0b"), r"\u000b"); // vertical tab
        assert_eq!(escape_json_string("\x0c"), r"\u000c"); // form feed
        assert_eq!(escape_json_string("\x1b"), r"\u001b"); // escape
        assert_eq!(escape_json_string("\x7f"), r"\u007f"); // DEL
    }

    #[test]
    fn test_escape_json_string_combined() {
        // A realistic error message with quotes and newlines
        let input = "ValueError: \"bad input\"\n  at line 5";
        let expected = r#"ValueError: \"bad input\"\n  at line 5"#;
        assert_eq!(escape_json_string(input), expected);
    }

    #[test]
    fn test_escape_json_string_unicode() {
        // Unicode should pass through unchanged
        assert_eq!(escape_json_string("héllo wörld 你好"), "héllo wörld 你好");
        assert_eq!(escape_json_string("emoji: 🎉"), "emoji: 🎉");
        // Various unicode categories
        assert_eq!(escape_json_string("αβγδ"), "αβγδ"); // Greek
        assert_eq!(escape_json_string("日本語"), "日本語"); // Japanese
        assert_eq!(escape_json_string("🎉🎊🎁"), "🎉🎊🎁"); // Emoji
        assert_eq!(escape_json_string("→←↑↓"), "→←↑↓"); // Arrows
    }

    // === Security-relevant edge cases ===

    #[test]
    fn test_escape_json_string_injection_attempts() {
        // Attempt to break out of JSON string with unescaped quote
        // Input: foo", "injected": "bar
        // Output: foo\", \"injected\": \"bar
        assert_eq!(
            escape_json_string("foo\", \"injected\": \"bar"),
            r#"foo\", \"injected\": \"bar"#
        );

        // Attempt to inject via backslash-quote sequence
        // Input: foo\", "x": "y  (backslash then quote then rest)
        // Output: foo\\\", \"x\": \"y
        assert_eq!(
            escape_json_string("foo\\\", \"x\": \"y"),
            r#"foo\\\", \"x\": \"y"#
        );

        // Nested escaping attempts
        // Input: \\" (two backslashes and a quote) -> \\\\" (four backslashes, escaped quote)
        assert_eq!(escape_json_string("\\\\\""), "\\\\\\\\\\\"");
        // Input: \\\" (three backslashes and a quote) -> \\\\\\" (six backslashes, escaped quote)
        assert_eq!(escape_json_string("\\\\\\\""), "\\\\\\\\\\\\\\\"");
    }

    #[test]
    fn test_escape_json_string_newline_injection() {
        // Attempt to inject via newlines (could break JSON parsers)
        assert_eq!(
            escape_json_string("line1\n\"injected\": true"),
            r#"line1\n\"injected\": true"#
        );

        // CRLF injection
        assert_eq!(
            escape_json_string("line1\r\n\"injected\": true"),
            r#"line1\r\n\"injected\": true"#
        );
    }

    #[test]
    fn test_escape_json_string_null_byte_injection() {
        // Null bytes could cause issues with C-style string handling
        assert_eq!(escape_json_string("before\x00after"), r"before\u0000after");
        assert_eq!(escape_json_string("\x00\x00\x00"), r"\u0000\u0000\u0000");
        assert_eq!(escape_json_string("data\x00"), r"data\u0000");
    }

    #[test]
    fn test_escape_json_string_unicode_escapes() {
        // Literal \uXXXX in input (should escape the backslash)
        assert_eq!(escape_json_string(r"\u0000"), r"\\u0000");
        assert_eq!(escape_json_string(r"\u003c"), r"\\u003c");

        // This prevents attackers from using literal \uXXXX to bypass escaping
        assert_eq!(escape_json_string(r#"\u0022"#), r#"\\u0022"#); // \u0022 = "
    }

    #[test]
    fn test_escape_json_string_html_in_json() {
        // HTML/script injection (JSON inside HTML context)
        // Note: JSON spec doesn't require escaping < > & but we pass them through
        // The consumer should HTML-escape if needed
        assert_eq!(
            escape_json_string("<script>alert(1)</script>"),
            "<script>alert(1)</script>"
        );
        assert_eq!(escape_json_string("</script><script>"), "</script><script>");

        // But quotes are still escaped
        assert_eq!(
            escape_json_string(r#"<img onerror="alert(1)">"#),
            r#"<img onerror=\"alert(1)\">"#
        );
    }

    #[test]
    fn test_escape_json_string_long_strings() {
        // Very long string
        let long_input = "a".repeat(10000);
        let result = escape_json_string(&long_input);
        assert_eq!(result.len(), 10000);
        assert_eq!(result, long_input);

        // Long string with characters that need escaping
        let long_with_escapes = "a\nb".repeat(1000);
        let result = escape_json_string(&long_with_escapes);
        assert_eq!(result, r"a\nb".repeat(1000));
    }

    #[test]
    fn test_escape_json_string_only_special_chars() {
        // String of only special characters
        assert_eq!(escape_json_string("\"\"\"\n\n\n"), r#"\"\"\"\n\n\n"#);
        assert_eq!(escape_json_string("\\\\\\\t\t\t"), r"\\\\\\\t\t\t");
    }

    #[test]
    fn test_escape_json_string_realistic_error_messages() {
        // Python traceback
        let traceback = r#"Traceback (most recent call last):
  File "test.py", line 10, in <module>
    raise ValueError("invalid \"input\"")
ValueError: invalid "input""#;
        let escaped = escape_json_string(traceback);
        assert!(escaped.contains(r#"\"input\""#));
        assert!(escaped.contains(r"\n"));
        assert!(!escaped.contains('\n')); // No literal newlines

        // Error with file paths (Windows-style)
        let win_path = r#"Error: Cannot open "C:\Users\test\file.txt""#;
        let escaped = escape_json_string(win_path);
        assert_eq!(
            escaped,
            r#"Error: Cannot open \"C:\\Users\\test\\file.txt\""#
        );
    }

    #[test]
    fn test_escape_json_string_result_is_valid_json() {
        // Verify that wrapping the result in quotes produces valid JSON
        let test_cases = [
            "hello",
            "with \"quotes\"",
            "with\nnewlines",
            "with\ttabs",
            "with\\backslashes",
            "\x00null\x00bytes",
            "unicode: 🎉",
            r#"complex: "foo\nbar""#,
        ];

        for input in test_cases {
            let escaped = escape_json_string(input);
            let json_string = format!("\"{}\"", escaped);

            // This should be valid JSON that parses back to the original
            // We can't use serde_json here (not a dependency), but we can
            // at least verify basic structure
            assert!(json_string.starts_with('"'));
            assert!(json_string.ends_with('"'));
            // No unescaped quotes in the middle
            let inner = &json_string[1..json_string.len() - 1];
            let mut chars = inner.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '"' {
                    panic!("Unescaped quote in JSON string: {}", json_string);
                }
                if c == '\\' {
                    // Must be followed by a valid escape character
                    let next = chars.next().expect("Trailing backslash");
                    assert!(
                        matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u'),
                        "Invalid escape sequence: \\{} in {}",
                        next,
                        json_string
                    );
                    if next == 'u' {
                        // Must be followed by 4 hex digits
                        for _ in 0..4 {
                            let hex = chars.next().expect("Incomplete \\uXXXX");
                            assert!(hex.is_ascii_hexdigit(), "Invalid hex in \\uXXXX");
                        }
                    }
                }
            }
        }
    }
}
