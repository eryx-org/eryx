//! CPython FFI bindings for eryx-wasm-runtime.
//!
//! This module uses pyo3::ffi for CPython C API bindings where available in the
//! stable ABI (abi3), with manual declarations for functions not exposed there.
//!
//! These symbols are resolved at component link time when we link against libpython3.14.so.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(missing_docs)]
#![allow(missing_debug_implementations)]
// TODO: Update to PyErr_GetRaisedException when pyo3 exposes it in stable ABI
#![allow(deprecated)]

use std::ffi::{c_char, c_int};
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
    PyErr_Fetch,
    PyErr_NormalizeException,
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

// =============================================================================
// Additional CPython FFI declarations not in pyo3::ffi stable ABI
// =============================================================================

/// Python compiler flags structure.
#[repr(C)]
pub struct PyCompilerFlags {
    pub cf_flags: c_int,
    pub cf_feature_version: c_int,
}

unsafe extern "C" {
    // Code execution (not in stable ABI)
    pub fn PyRun_SimpleString(command: *const c_char) -> c_int;
    pub fn PyRun_SimpleStringFlags(command: *const c_char, flags: *mut PyCompilerFlags) -> c_int;
    pub fn PyRun_String(
        str: *const c_char,
        start: c_int,
        globals: *mut PyObject,
        locals: *mut PyObject,
    ) -> *mut PyObject;
    pub fn PyRun_StringFlags(
        str: *const c_char,
        start: c_int,
        globals: *mut PyObject,
        locals: *mut PyObject,
        flags: *mut PyCompilerFlags,
    ) -> *mut PyObject;

    // PyUnicode_AsUTF8 (not in stable ABI, use PyUnicode_AsUTF8AndSize instead if possible)
    pub fn PyUnicode_AsUTF8(unicode: *mut PyObject) -> *const c_char;

    // Singletons (not exposed as statics in pyo3::ffi stable ABI)
    pub static mut _Py_NoneStruct: PyObject;
    pub static mut _Py_TrueStruct: PyObject;
    pub static mut _Py_FalseStruct: PyObject;
}

// Start symbols for PyRun_String
pub const Py_eval_input: c_int = 258;
pub const Py_file_input: c_int = 257;
pub const Py_single_input: c_int = 256;

// =============================================================================
// Helper macros and inline functions
// =============================================================================

/// Get a pointer to Py_None.
///
/// # Safety
/// Python must be initialized.
#[inline]
pub unsafe fn Py_None() -> *mut PyObject {
    std::ptr::addr_of_mut!(_Py_NoneStruct)
}

/// Get a pointer to Py_True.
///
/// # Safety
/// Python must be initialized.
#[inline]
pub unsafe fn Py_True() -> *mut PyObject {
    std::ptr::addr_of_mut!(_Py_TrueStruct)
}

/// Get a pointer to Py_False.
///
/// # Safety
/// Python must be initialized.
#[inline]
pub unsafe fn Py_False() -> *mut PyObject {
    std::ptr::addr_of_mut!(_Py_FalseStruct)
}

/// Increment reference count.
///
/// # Safety
/// `op` must be a valid Python object pointer.
#[inline]
pub unsafe fn Py_INCREF(op: *mut PyObject) {
    unsafe { Py_IncRef(op) };
}

/// Decrement reference count.
///
/// # Safety
/// `op` must be a valid Python object pointer.
#[inline]
pub unsafe fn Py_DECREF(op: *mut PyObject) {
    unsafe { Py_DecRef(op) };
}

/// Decrement reference count, allowing NULL.
///
/// # Safety
/// `op` must be either NULL or a valid Python object pointer.
#[inline]
pub unsafe fn Py_XDECREF(op: *mut PyObject) {
    if !op.is_null() {
        unsafe { Py_DecRef(op) };
    }
}

/// Increment reference count, allowing NULL.
///
/// # Safety
/// `op` must be either NULL or a valid Python object pointer.
#[inline]
pub unsafe fn Py_XINCREF(op: *mut PyObject) {
    if !op.is_null() {
        unsafe { Py_IncRef(op) };
    }
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

use std::cell::RefCell;

// Thread-local storage for the invoke callbacks.
// These are set by lib.rs during export execution.
// Note: WASM is single-threaded, so RefCell is sufficient and avoids lock overhead.
thread_local! {
    static INVOKE_CALLBACK: RefCell<Option<InvokeCallback>> = const { RefCell::new(None) };
    static INVOKE_ASYNC_CALLBACK: RefCell<Option<InvokeAsyncCallback>> = const { RefCell::new(None) };
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

// =============================================================================
// Execute result type for async support
// =============================================================================

/// Callback codes from componentize_py_async_support
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

/// Result of executing Python code.
#[derive(Debug)]
pub enum ExecuteResult {
    /// Execution completed successfully with output.
    Complete(String),
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

/// Call Python's `componentize_py_async_support.callback(event0, event1, event2)`.
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
import componentize_py_async_support as _cpas
_eryx_callback_code = _cpas.callback({event0}, {event1}, {event2})
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
    use std::ffi::CString;

    // Capture the output and restore original stdout/stderr
    let code = r#"
import sys as _sys
if '_eryx_stdout' in dir():
    _eryx_output = _eryx_stdout.getvalue()
    _eryx_errors = _eryx_stderr.getvalue()
    _sys.stdout = _eryx_old_stdout
    _sys.stderr = _eryx_old_stderr
    del _eryx_stdout, _eryx_stderr, _eryx_old_stdout, _eryx_old_stderr
else:
    _eryx_output = ''
    _eryx_errors = ''
"#;
    if let Ok(code_cstr) = CString::new(code) {
        unsafe {
            if PyRun_SimpleString(code_cstr.as_ptr()) != 0 {
                PyErr_Clear();
            }
        }
    }
}

/// Store the result of an async import for Python's promise_get_result to read.
///
/// This is called from `export_async_callback` after lifting the result from the buffer.
/// The result is stored in `_eryx_async_import_result` in Python.
pub fn set_async_import_result(_subtask: u32, result_json: &str) {
    use std::ffi::CString;

    // Escape the JSON for embedding in Python
    let escaped = result_json.replace('\\', "\\\\").replace('"', "\\\"");

    let code = format!(r#"_eryx_async_import_result = "{escaped}""#);

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
// They match the interface expected by componentize_py_async_support.

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

/// The _eryx module definition.
/// This generates a `PyInit__eryx` function that can be registered with Python.
#[pymodule]
#[pyo3(name = "_eryx")]
fn eryx_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_eryx_invoke, m)?)?;
    m.add_function(wrap_pyfunction!(_eryx_invoke_async, m)?)?;
    // Async support functions (matching componentize_py_runtime interface)
    m.add_function(wrap_pyfunction!(waitable_set_new_, m)?)?;
    m.add_function(wrap_pyfunction!(waitable_set_drop_, m)?)?;
    m.add_function(wrap_pyfunction!(waitable_join_, m)?)?;
    m.add_function(wrap_pyfunction!(context_set_, m)?)?;
    m.add_function(wrap_pyfunction!(context_get_, m)?)?;
    m.add_function(wrap_pyfunction!(subtask_drop_, m)?)?;
    Ok(())
}

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
/// - Ensures __main__ module exists for code execution
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

        let mut ptype: *mut PyObject = std::ptr::null_mut();
        let mut pvalue: *mut PyObject = std::ptr::null_mut();
        let mut ptraceback: *mut PyObject = std::ptr::null_mut();

        PyErr_Fetch(&mut ptype, &mut pvalue, &mut ptraceback);

        if pvalue.is_null() {
            Py_XDECREF(ptype);
            Py_XDECREF(ptraceback);
            return "Unknown error (no value)".to_string();
        }

        // Normalize the exception
        PyErr_NormalizeException(&mut ptype, &mut pvalue, &mut ptraceback);

        // Try to get string representation of the error value
        let str_obj = PyObject_Str(pvalue);
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
            Py_DECREF(str_obj);
            msg
        };

        Py_XDECREF(ptype);
        Py_XDECREF(pvalue);
        Py_XDECREF(ptraceback);

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
            Py_DECREF(str_obj);
            return Err("Failed to get UTF-8 from string".to_string());
        } else {
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned()
        };

        Py_DECREF(str_obj);
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
        // Set up stdout/stderr capture and async execution infrastructure
        // Also patch socket.socketpair to work in WASI (needed for asyncio)
        let capture_setup = c"
import sys as _sys
from io import StringIO as _StringIO
import ast as _ast
import types as _types

# Patch socket.socketpair before importing asyncio
# WASI doesn't support socketpair, so we create a dummy that works for asyncio's self-pipe
import socket as _socket
_socket_original_socketpair = getattr(_socket, 'socketpair', None)

class _DummySocket:
    '''Dummy socket for asyncio self-pipe in WASI.'''
    def __init__(self):
        self._buffer = []
        self._closed = False
    def fileno(self):
        return -1  # Invalid fd, but asyncio might not check
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
    '''Dummy socketpair for WASI asyncio compatibility.'''
    return (_DummySocket(), _DummySocket())

# Always patch socketpair in WASI environment
_socket.socketpair = _dummy_socketpair

_eryx_stdout = _StringIO()
_eryx_stderr = _StringIO()
_eryx_old_stdout = _sys.stdout
_eryx_old_stderr = _sys.stderr
_sys.stdout = _eryx_stdout
_sys.stderr = _eryx_stderr
";
        if PyRun_SimpleString(capture_setup.as_ptr()) != 0 {
            PyErr_Clear();
            return ExecuteResult::Error("Failed to set up output capture".to_string());
        }

        // Build the execution code - compile with TLA support and run with async support
        // We try to use componentize_py_async_support for proper async handling,
        // falling back to a simple coroutine runner if not available
        let exec_wrapper = format!(
            r#"
# Try to import async support for proper pending handling
try:
    import componentize_py_async_support as _cpas
    import asyncio as _asyncio
    _CPAS_AVAILABLE = True
except ImportError:
    _CPAS_AVAILABLE = False

# Global to store the callback code from first_poll
_eryx_callback_code = 0  # 0 = EXIT (complete), 2 = WAIT (pending), etc.

def _eryx_run_async(coro):
    '''Coroutine runner with componentize_py_async_support integration.

    Uses the custom event loop from componentize_py_async_support when available,
    which properly handles pending async operations and the Component Model callback protocol.

    Stores the callback code in _eryx_callback_code global:
    - 0 (EXIT): Execution complete
    - 2 (WAIT) | (waitable_set << 4): Execution pending, need to wait
    '''
    global _eryx_callback_code

    if _CPAS_AVAILABLE:
        # The module already initializes _loop at import time
        # Just set it as the current asyncio event loop
        _asyncio.set_event_loop(_cpas._loop)

        # Run the coroutine using first_poll which sets up the async context
        # export_index=0, borrows=0 - these are for async export tracking
        # which we don't need for execute()
        async def _wrapper():
            return await coro

        _eryx_callback_code = _cpas.first_poll(0, 0, _wrapper())
        return None
    else:
        _eryx_callback_code = 0  # No async support, always complete
        # Fallback: Simple coroutine runner for WASI
        try:
            result = coro.send(None)
            while True:
                if hasattr(result, 'send'):
                    inner_result = _eryx_run_async(result)
                    result = coro.send(inner_result)
                elif hasattr(result, '__await__'):
                    inner_result = _eryx_run_async(result.__await__())
                    result = coro.send(inner_result)
                else:
                    result = coro.send(result)
        except StopIteration as e:
            return e.value

_eryx_user_code = {}
_eryx_compiled = compile(_eryx_user_code, '<user>', 'exec', flags=_ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)

# Check if the compiled code is a coroutine (has top-level await)
if _eryx_compiled.co_flags & 0x80:  # CO_COROUTINE
    # Create a function from the code and run it as a coroutine
    _eryx_fn = _types.FunctionType(_eryx_compiled, globals())
    _eryx_coro = _eryx_fn()
    _eryx_run_async(_eryx_coro)
else:
    # Regular synchronous code
    exec(_eryx_compiled, globals())
"#,
            python_string_literal(code)
        );

        let exec_code_cstr = match CString::new(exec_wrapper) {
            Ok(s) => s,
            Err(e) => return ExecuteResult::Error(format!("Invalid exec code: {e}")),
        };
        let exec_result = PyRun_SimpleString(exec_code_cstr.as_ptr());

        if exec_result != 0 {
            // Execution failed - teardown and get error
            let capture_teardown = c"
_sys.stdout = _eryx_old_stdout
_sys.stderr = _eryx_old_stderr
_eryx_output = _eryx_stdout.getvalue()
_eryx_errors = _eryx_stderr.getvalue()
del _eryx_stdout, _eryx_stderr, _eryx_old_stdout, _eryx_old_stderr
";
            let _ = PyRun_SimpleString(capture_teardown.as_ptr());

            let stderr_output = get_python_variable_string("_eryx_errors").unwrap_or_default();
            let exception_msg = get_last_error_message();

            let error = if !stderr_output.is_empty() && exception_msg != "Unknown error" {
                format!("{stderr_output}\n{exception_msg}")
            } else if !stderr_output.is_empty() {
                stderr_output
            } else {
                exception_msg
            };

            let _ = PyRun_SimpleString(c"del _eryx_output, _eryx_errors".as_ptr());
            return ExecuteResult::Error(error);
        }

        // Check the callback code BEFORE teardown to see if execution is pending
        let callback_code_value = get_python_callback_code();
        let code = callback_code::get_code(callback_code_value);

        if code == callback_code::WAIT {
            // Execution is pending - keep capture variables including stdout redirection
            // so that async code can still print and we'll capture it later
            return ExecuteResult::Pending(callback_code_value);
        }

        // Execution complete - full teardown
        let capture_teardown = c"
_sys.stdout = _eryx_old_stdout
_sys.stderr = _eryx_old_stderr
_eryx_output = _eryx_stdout.getvalue()
_eryx_errors = _eryx_stderr.getvalue()
del _eryx_stdout, _eryx_stderr, _eryx_old_stdout, _eryx_old_stderr
";
        if PyRun_SimpleString(capture_teardown.as_ptr()) != 0 {
            PyErr_Clear();
        }

        let output = get_python_variable_string("_eryx_output").unwrap_or_default();
        let _ = PyRun_SimpleString(c"del _eryx_output, _eryx_errors".as_ptr());

        ExecuteResult::Complete(output.trim_end_matches('\n').to_string())
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
        Py_DECREF(bytes_obj); // SetItem increments ref, so we decrement ours

        if result != 0 {
            let err = get_last_error_message();
            return Err(format!("Failed to set variable '{name}': {err}"));
        }

        Ok(())
    }
}

/// Snapshot the current Python state by pickling `__main__.__dict__`.
///
/// Returns the pickled state as bytes, which can be restored later with `restore_state`.
///
/// # What is preserved
/// - All user-defined variables in `__main__`
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
        // Pickle __main__.__dict__, excluding unpicklable items
        let pickle_code = c"
import pickle as _eryx_pickle
import __main__ as _eryx_main

# Items to exclude from snapshot (builtins and our temp vars)
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
                if cell.cell_contents == invoke:
                    return True
            except (ValueError, NameError):
                pass
    # Check for namespace objects
    obj_type = type(obj).__name__
    if obj_type in ('_EryxNamespace', '_EryxCallbackLeaf'):
        return True
    return False

# Take a snapshot of the keys first to avoid 'dictionary changed size during iteration'
_eryx_keys = list(_eryx_main.__dict__.keys())

# Build dict of picklable items
_eryx_state_dict = {}
for _k in _eryx_keys:
    if _k not in _eryx_exclude and not _k.startswith('_eryx_'):
        _v = _eryx_main.__dict__.get(_k)
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
";

        if PyRun_SimpleString(pickle_code.as_ptr()) != 0 {
            let err = get_last_error_message();
            let _ = PyRun_SimpleString(
                c"del _eryx_pickle, _eryx_main, _eryx_exclude, _eryx_is_callback_obj, _eryx_keys, _eryx_state_dict"
                    .as_ptr(),
            );
            return Err(format!("Failed to snapshot state: {err}"));
        }

        // Get the pickled bytes
        let state_bytes = get_python_variable_bytes("_eryx_state_bytes")?;

        // Clean up
        let _ = PyRun_SimpleString(
            c"del _eryx_pickle, _eryx_main, _eryx_exclude, _eryx_is_callback_obj, _eryx_keys, _eryx_state_dict, _eryx_state_bytes"
                .as_ptr(),
        );

        Ok(state_bytes)
    }
}

/// Restore Python state from a previous snapshot.
///
/// This unpickles the data and updates `__main__.__dict__` with the restored values.
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

        // Unpickle and update __main__.__dict__
        let restore_code = c"
import pickle as _eryx_pickle
import __main__ as _eryx_main

# Unpickle the state
_eryx_restored_dict = _eryx_pickle.loads(_eryx_restore_bytes)

# Update __main__ with restored values
_eryx_main.__dict__.update(_eryx_restored_dict)

# Clean up
del _eryx_restore_bytes, _eryx_restored_dict, _eryx_pickle, _eryx_main
";

        if PyRun_SimpleString(restore_code.as_ptr()) != 0 {
            let err = get_last_error_message();
            let _ = PyRun_SimpleString(c"del _eryx_restore_bytes".as_ptr());
            return Err(format!("Failed to restore state: {err}"));
        }

        Ok(())
    }
}

/// Clear all user-defined state from `__main__`.
///
/// This removes all variables except Python builtins and module metadata,
/// effectively resetting to a fresh interpreter state.
pub fn clear_state() {
    if !is_python_initialized() {
        return;
    }

    unsafe {
        let clear_code = c"
import __main__ as _eryx_main

# Items to keep (builtins and module metadata)
_eryx_keep = {
    '__builtins__', '__name__', '__doc__', '__package__',
    '__loader__', '__spec__', '__cached__', '__file__',
    '_eryx_main', '_eryx_keep', '_eryx_to_delete',
    # Preserve callback infrastructure
    'invoke', 'list_callbacks', '_EryxNamespace', '_EryxCallbackLeaf',
    '_eryx_make_callback', '_eryx_reserved', '_eryx_callbacks',
    # Preserve json module alias used by list_callbacks and _eryx module for invoke()
    '_json', '_eryx',
    # Preserve async support modules
    '_cpr', '_cpas', '_Ok', '_Err', '_ASYNC_SUPPORT',
}

# Also keep callback wrappers and namespace objects
def _eryx_should_keep(k, v):
    if k in _eryx_keep:
        return True
    if k.startswith('_eryx_'):
        return True
    # Keep callback wrapper functions
    if callable(v) and hasattr(v, '__closure__') and v.__closure__:
        for cell in v.__closure__:
            try:
                if 'invoke' in dir() and cell.cell_contents == invoke:
                    return True
            except (ValueError, NameError):
                pass
    # Keep namespace objects
    if type(v).__name__ in ('_EryxNamespace', '_EryxCallbackLeaf'):
        return True
    return False

# Collect keys to delete (can't modify dict during iteration)
_eryx_to_delete = [k for k, v in list(_eryx_main.__dict__.items()) if not _eryx_should_keep(k, v)]

# Delete the keys
for _k in _eryx_to_delete:
    del _eryx_main.__dict__[_k]

# Clean up our temporaries
del _eryx_main, _eryx_keep, _eryx_should_keep, _eryx_to_delete, _k
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

# Try to import async support, but don't fail if unavailable
try:
    import componentize_py_runtime as _cpr
    import componentize_py_async_support as _cpas
    from componentize_py_types import Ok as _Ok, Err as _Err
    _ASYNC_SUPPORT = True
except ImportError:
    _ASYNC_SUPPORT = False

# Callbacks metadata from host
_eryx_callbacks_json = '''{}'''
_eryx_callbacks = _json.loads(_eryx_callbacks_json)

async def invoke(name, **kwargs):
    """Invoke a host callback by name with keyword arguments (async).

    Args:
        name: Name of the callback (e.g., "sleep", "http.get")
        **kwargs: Arguments to pass to the callback

    Returns:
        The callback result (parsed from JSON)

    Example:
        result = await invoke("get_time")
        data = await invoke("http.get", url="https://example.com")
    """
    # Serialize kwargs to JSON
    args_json = _json.dumps(kwargs)

    if _ASYNC_SUPPORT:
        # Use async-aware invoke that can handle pending operations
        result = _cpr.invoke_async(name, args_json)
        if isinstance(result, _Ok):
            result_json = result.value
        elif isinstance(result, _Err):
            # Check if this is a pending tuple or an error string
            if isinstance(result.value, tuple):
                # Pending - wait for it using await_result
                result_json = await _cpas.await_result(result)
            else:
                # Actual error
                raise RuntimeError(result.value)
        if result_json:
            return _json.loads(result_json)
        return None
    else:
        # Fallback to synchronous invoke
        result_json = _eryx._eryx_invoke(name, args_json)
        if result_json:
            return _json.loads(result_json)
        return None

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
def _eryx_make_callback(name):
    async def callback(**kwargs):
        # invoke() is now async, so await it
        return await invoke(name, **kwargs)
    callback.__name__ = name
    callback.__doc__ = f"Invoke the '{{name}}' callback (async)."
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

# Generate callback wrappers
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
            globals()[_name] = _eryx_make_callback(_name)

# Add namespaces to globals
globals().update(_eryx_namespaces)

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
fn escape_json_string(s: &str) -> String {
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
    // Tests will be added when we can actually run Python
    // For now, just verify the module compiles
}
