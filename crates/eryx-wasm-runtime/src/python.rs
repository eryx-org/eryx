//! CPython FFI bindings for eryx-wasm-runtime.
//!
//! This module declares the minimal CPython C API surface needed for the eryx sandbox.
//! These symbols are resolved at component link time when we link against libpython3.14.so.
//!
//! We declare them as `extern "C"` rather than using pyo3-ffi because:
//! 1. We're compiling to a wasm32-wasip1 core module, not the host platform
//! 2. The Python symbols come from the WASM-compiled libpython, not the host Python
//! 3. We don't need the full pyo3 machinery, just a few core functions

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(missing_docs)]
#![allow(missing_debug_implementations)]

use std::ffi::{c_char, c_int, c_long, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

/// Opaque Python object pointer.
/// All Python objects are represented as pointers to this type.
#[repr(C)]
pub struct PyObject {
    _private: [u8; 0],
}

/// Python compiler flags structure.
/// Used by PyRun_StringFlags and similar functions.
#[repr(C)]
pub struct PyCompilerFlags {
    pub cf_flags: c_int,
    pub cf_feature_version: c_int,
}

// =============================================================================
// CPython C API declarations
// =============================================================================
//
// These symbols are provided by libpython3.14.so at component link time.
// They follow the Python Stable ABI where possible.

unsafe extern "C" {
    // -------------------------------------------------------------------------
    // Interpreter lifecycle
    // -------------------------------------------------------------------------

    /// Initialize the Python interpreter.
    /// Must be called before using any other Python/C API functions.
    pub fn Py_Initialize();

    /// Initialize the Python interpreter with optional signal handler setup.
    /// - initsigs = 0: Skip signal handler initialization (recommended for embedding)
    /// - initsigs = 1: Register signal handlers (like Py_Initialize)
    pub fn Py_InitializeEx(initsigs: c_int);

    /// Check if the Python interpreter is initialized.
    /// Returns non-zero if initialized, zero otherwise.
    pub fn Py_IsInitialized() -> c_int;

    /// Finalize the Python interpreter.
    /// Returns 0 on success, -1 if errors occurred during finalization.
    pub fn Py_FinalizeEx() -> c_int;

    // -------------------------------------------------------------------------
    // Code execution
    // -------------------------------------------------------------------------

    /// Execute Python source code in the __main__ module.
    /// Returns 0 on success, -1 if an exception was raised.
    pub fn PyRun_SimpleString(command: *const c_char) -> c_int;

    /// Execute Python source code with compiler flags.
    /// Returns 0 on success, -1 if an exception was raised.
    pub fn PyRun_SimpleStringFlags(command: *const c_char, flags: *mut PyCompilerFlags) -> c_int;

    /// Execute Python source code and return the result.
    ///
    /// - str: Python source code
    /// - start: Start symbol (Py_eval_input, Py_file_input, Py_single_input)
    /// - globals: Global namespace (must be a dict)
    /// - locals: Local namespace (any mapping)
    ///
    /// Returns new reference to result, or NULL on error.
    pub fn PyRun_String(
        str: *const c_char,
        start: c_int,
        globals: *mut PyObject,
        locals: *mut PyObject,
    ) -> *mut PyObject;

    /// Execute Python source code with compiler flags.
    pub fn PyRun_StringFlags(
        str: *const c_char,
        start: c_int,
        globals: *mut PyObject,
        locals: *mut PyObject,
        flags: *mut PyCompilerFlags,
    ) -> *mut PyObject;

    // -------------------------------------------------------------------------
    // Exception handling
    // -------------------------------------------------------------------------

    /// Test whether the error indicator is set.
    /// Returns borrowed reference to exception type, or NULL if no error.
    pub fn PyErr_Occurred() -> *mut PyObject;

    /// Clear the error indicator.
    pub fn PyErr_Clear();

    /// Print standard traceback to sys.stderr and clear error indicator.
    pub fn PyErr_Print();

    /// Print traceback and optionally set sys.last_* variables.
    pub fn PyErr_PrintEx(set_sys_last_vars: c_int);

    /// Set the error indicator with a string message.
    pub fn PyErr_SetString(type_: *mut PyObject, message: *const c_char);

    /// Retrieve the error indicator (deprecated in 3.12, but still available).
    /// Clears the error indicator and returns references via output parameters.
    pub fn PyErr_Fetch(
        ptype: *mut *mut PyObject,
        pvalue: *mut *mut PyObject,
        ptraceback: *mut *mut PyObject,
    );

    /// Normalize a fetched exception (prepares it for use).
    pub fn PyErr_NormalizeException(
        exc: *mut *mut PyObject,
        val: *mut *mut PyObject,
        tb: *mut *mut PyObject,
    );

    // -------------------------------------------------------------------------
    // Object protocol
    // -------------------------------------------------------------------------

    /// Get string representation of object (like str(o)).
    /// Returns new reference.
    pub fn PyObject_Str(o: *mut PyObject) -> *mut PyObject;

    /// Get repr representation of object (like repr(o)).
    /// Returns new reference.
    pub fn PyObject_Repr(o: *mut PyObject) -> *mut PyObject;

    /// Get attribute by name (like o.attr_name).
    /// Returns new reference, or NULL on error.
    pub fn PyObject_GetAttrString(o: *mut PyObject, attr_name: *const c_char) -> *mut PyObject;

    /// Set attribute by name (like o.attr_name = v).
    /// Returns 0 on success, -1 on error.
    pub fn PyObject_SetAttrString(
        o: *mut PyObject,
        attr_name: *const c_char,
        v: *mut PyObject,
    ) -> c_int;

    /// Call a callable object with arguments.
    /// args should be a tuple, kwargs a dict (or NULL).
    /// Returns new reference.
    pub fn PyObject_Call(
        callable: *mut PyObject,
        args: *mut PyObject,
        kwargs: *mut PyObject,
    ) -> *mut PyObject;

    /// Call a callable with no arguments.
    /// Returns new reference.
    pub fn PyObject_CallNoArgs(callable: *mut PyObject) -> *mut PyObject;

    // -------------------------------------------------------------------------
    // Reference counting
    // -------------------------------------------------------------------------

    /// Increment reference count.
    pub fn Py_IncRef(o: *mut PyObject);

    /// Decrement reference count.
    pub fn Py_DecRef(o: *mut PyObject);

    // -------------------------------------------------------------------------
    // Module operations
    // -------------------------------------------------------------------------

    /// Import a module by name.
    /// Returns new reference to the module, or NULL on error.
    pub fn PyImport_ImportModule(name: *const c_char) -> *mut PyObject;

    /// Get the __main__ module.
    /// Returns borrowed reference.
    pub fn PyImport_AddModule(name: *const c_char) -> *mut PyObject;

    /// Get a module's __dict__.
    /// Returns borrowed reference.
    pub fn PyModule_GetDict(module: *mut PyObject) -> *mut PyObject;

    // -------------------------------------------------------------------------
    // String/Unicode operations
    // -------------------------------------------------------------------------

    /// Create a Unicode string from a UTF-8 encoded C string.
    /// Returns new reference.
    pub fn PyUnicode_FromString(str: *const c_char) -> *mut PyObject;

    /// Create a Unicode string from a UTF-8 encoded buffer with length.
    /// Returns new reference.
    pub fn PyUnicode_FromStringAndSize(str: *const c_char, size: isize) -> *mut PyObject;

    /// Get UTF-8 encoded content of a Unicode string.
    /// Returns pointer to internal buffer (do not modify or free).
    /// The pointer is valid as long as the PyObject exists.
    pub fn PyUnicode_AsUTF8(unicode: *mut PyObject) -> *const c_char;

    /// Get UTF-8 encoded content with length.
    /// Returns pointer and sets size.
    pub fn PyUnicode_AsUTF8AndSize(unicode: *mut PyObject, size: *mut isize) -> *const c_char;

    // -------------------------------------------------------------------------
    // Bytes operations
    // -------------------------------------------------------------------------

    /// Create a bytes object from a buffer.
    /// Returns new reference.
    pub fn PyBytes_FromStringAndSize(str: *const c_char, size: isize) -> *mut PyObject;

    /// Get pointer to the internal buffer of a bytes object.
    /// Returns pointer to internal buffer (do not modify or free).
    pub fn PyBytes_AsString(o: *mut PyObject) -> *mut c_char;

    /// Get the size of a bytes object.
    pub fn PyBytes_Size(o: *mut PyObject) -> isize;

    /// Get both pointer and size of a bytes object.
    /// Returns 0 on success, -1 on error.
    pub fn PyBytes_AsStringAndSize(
        o: *mut PyObject,
        buffer: *mut *mut c_char,
        length: *mut isize,
    ) -> c_int;

    // -------------------------------------------------------------------------
    // Dict operations
    // -------------------------------------------------------------------------

    /// Create a new empty dictionary.
    /// Returns new reference.
    pub fn PyDict_New() -> *mut PyObject;

    /// Get item from dictionary by key.
    /// Returns borrowed reference, or NULL if not found (no exception set).
    pub fn PyDict_GetItem(dict: *mut PyObject, key: *mut PyObject) -> *mut PyObject;

    /// Get item from dictionary by string key.
    /// Returns borrowed reference, or NULL if not found.
    pub fn PyDict_GetItemString(dict: *mut PyObject, key: *const c_char) -> *mut PyObject;

    /// Set item in dictionary.
    /// Returns 0 on success, -1 on error.
    pub fn PyDict_SetItem(dict: *mut PyObject, key: *mut PyObject, val: *mut PyObject) -> c_int;

    /// Set item in dictionary by string key.
    /// Returns 0 on success, -1 on error.
    pub fn PyDict_SetItemString(
        dict: *mut PyObject,
        key: *const c_char,
        val: *mut PyObject,
    ) -> c_int;

    /// Copy a dictionary (shallow copy).
    /// Returns new reference.
    pub fn PyDict_Copy(dict: *mut PyObject) -> *mut PyObject;

    /// Clear all items from dictionary.
    pub fn PyDict_Clear(dict: *mut PyObject);

    /// Update dictionary with items from another mapping.
    /// Returns 0 on success, -1 on error.
    pub fn PyDict_Update(dict: *mut PyObject, other: *mut PyObject) -> c_int;

    // -------------------------------------------------------------------------
    // Tuple operations
    // -------------------------------------------------------------------------

    /// Create a new tuple of given size.
    /// Returns new reference.
    pub fn PyTuple_New(size: isize) -> *mut PyObject;

    /// Set item in tuple (steals reference to v).
    /// Returns 0 on success, -1 on error.
    /// Only use on newly created tuples!
    pub fn PyTuple_SetItem(tuple: *mut PyObject, pos: isize, v: *mut PyObject) -> c_int;

    // -------------------------------------------------------------------------
    // List operations
    // -------------------------------------------------------------------------

    /// Create a new list of given size.
    /// Returns new reference.
    pub fn PyList_New(size: isize) -> *mut PyObject;

    /// Append an item to a list.
    /// Returns 0 on success, -1 on error.
    pub fn PyList_Append(list: *mut PyObject, item: *mut PyObject) -> c_int;

    // -------------------------------------------------------------------------
    // Long (int) operations
    // -------------------------------------------------------------------------

    /// Create a Python int from a C long.
    /// Returns new reference.
    pub fn PyLong_FromLong(v: c_long) -> *mut PyObject;

    /// Convert a Python int to a C long.
    /// Returns -1 on error (check PyErr_Occurred).
    pub fn PyLong_AsLong(o: *mut PyObject) -> c_long;

    // -------------------------------------------------------------------------
    // None and boolean singletons
    // -------------------------------------------------------------------------

    /// The None singleton (borrowed reference).
    pub static mut _Py_NoneStruct: PyObject;

    /// The True singleton (borrowed reference).
    pub static mut _Py_TrueStruct: PyObject;

    /// The False singleton (borrowed reference).
    pub static mut _Py_FalseStruct: PyObject;

    // -------------------------------------------------------------------------
    // Exception types
    // -------------------------------------------------------------------------

    pub static mut PyExc_BaseException: *mut PyObject;
    pub static mut PyExc_Exception: *mut PyObject;
    pub static mut PyExc_RuntimeError: *mut PyObject;
    pub static mut PyExc_TypeError: *mut PyObject;
    pub static mut PyExc_ValueError: *mut PyObject;
    pub static mut PyExc_KeyError: *mut PyObject;
    pub static mut PyExc_IndexError: *mut PyObject;
    pub static mut PyExc_AttributeError: *mut PyObject;
    pub static mut PyExc_MemoryError: *mut PyObject;
    pub static mut PyExc_SystemExit: *mut PyObject;

    // -------------------------------------------------------------------------
    // Built-in module creation
    // -------------------------------------------------------------------------

    /// Register a built-in module before Py_Initialize.
    /// Must be called BEFORE Py_Initialize.
    /// Returns 0 on success, -1 on error.
    pub fn PyImport_AppendInittab(
        name: *const c_char,
        initfunc: Option<unsafe extern "C" fn() -> *mut PyObject>,
    ) -> c_int;

    /// Create a new module object.
    /// Returns new reference.
    pub fn PyModule_Create2(module: *mut PyModuleDef, apiver: c_int) -> *mut PyObject;

    /// Add an object to a module with the given name.
    /// Returns 0 on success, -1 on error.
    /// Steals a reference to value on success.
    pub fn PyModule_AddObject(
        module: *mut PyObject,
        name: *const c_char,
        value: *mut PyObject,
    ) -> c_int;

    /// Add an object to a module (newer API, doesn't steal reference).
    /// Returns 0 on success, -1 on error.
    pub fn PyModule_AddObjectRef(
        module: *mut PyObject,
        name: *const c_char,
        value: *mut PyObject,
    ) -> c_int;

    /// Get the size of a tuple.
    pub fn PyTuple_Size(tuple: *mut PyObject) -> isize;

    /// Get an item from a tuple (borrowed reference).
    pub fn PyTuple_GetItem(tuple: *mut PyObject, pos: isize) -> *mut PyObject;
}

// =============================================================================
// Module definition structures (for PyModule_Create)
// =============================================================================

/// Python method definition for module functions.
#[repr(C)]
pub struct PyMethodDef {
    /// Method name (null-terminated).
    pub ml_name: *const c_char,
    /// C function pointer.
    pub ml_meth: Option<PyCFunction>,
    /// Flags indicating calling convention.
    pub ml_flags: c_int,
    /// Docstring (null-terminated, or NULL).
    pub ml_doc: *const c_char,
}

/// C function signature for METH_VARARGS | METH_KEYWORDS.
pub type PyCFunction =
    unsafe extern "C" fn(self_: *mut PyObject, args: *mut PyObject) -> *mut PyObject;

/// C function signature for METH_VARARGS | METH_KEYWORDS.
pub type PyCFunctionWithKeywords = unsafe extern "C" fn(
    self_: *mut PyObject,
    args: *mut PyObject,
    kwargs: *mut PyObject,
) -> *mut PyObject;

/// Method flags.
pub const METH_VARARGS: c_int = 0x0001;
pub const METH_KEYWORDS: c_int = 0x0002;
pub const METH_NOARGS: c_int = 0x0004;
pub const METH_O: c_int = 0x0008;

/// Sentinel for end of method definitions array.
pub const fn PyMethodDef_SENTINEL() -> PyMethodDef {
    PyMethodDef {
        ml_name: std::ptr::null(),
        ml_meth: None,
        ml_flags: 0,
        ml_doc: std::ptr::null(),
    }
}

/// Python module definition structure.
#[repr(C)]
pub struct PyModuleDef {
    pub m_base: PyModuleDef_Base,
    pub m_name: *const c_char,
    pub m_doc: *const c_char,
    pub m_size: isize,
    pub m_methods: *mut PyMethodDef,
    pub m_slots: *mut PyModuleDef_Slot,
    pub m_traverse: Option<unsafe extern "C" fn(*mut PyObject, *mut c_void, *mut c_void) -> c_int>,
    pub m_clear: Option<unsafe extern "C" fn(*mut PyObject) -> c_int>,
    pub m_free: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// Module definition base (opaque).
#[repr(C)]
pub struct PyModuleDef_Base {
    pub ob_base: PyObject,
    pub m_init: Option<unsafe extern "C" fn() -> *mut PyObject>,
    pub m_index: isize,
    pub m_copy: *mut PyObject,
}

/// Module definition slot (for multi-phase init).
#[repr(C)]
pub struct PyModuleDef_Slot {
    pub slot: c_int,
    pub value: *mut c_void,
}

/// Module API version constant.
pub const PYTHON_API_VERSION: c_int = 1013;

/// Helper to create PyModuleDef_Base with zeroed values.
pub const fn PyModuleDef_HEAD_INIT() -> PyModuleDef_Base {
    PyModuleDef_Base {
        ob_base: PyObject { _private: [] },
        m_init: None,
        m_index: 0,
        m_copy: std::ptr::null_mut(),
    }
}

// =============================================================================
// Start symbols for PyRun_String
// =============================================================================

/// For PyRun_String: evaluate a single expression (like eval())
pub const Py_eval_input: c_int = 258;

/// For PyRun_String: execute a sequence of statements (like exec())
pub const Py_file_input: c_int = 257;

/// For PyRun_String: execute a single interactive statement
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

/// Global storage for the invoke callback.
/// This is set by lib.rs during export execution.
static INVOKE_CALLBACK: std::sync::RwLock<Option<InvokeCallback>> = std::sync::RwLock::new(None);

/// Set the invoke callback function.
/// This should be called by lib.rs before executing Python code.
pub fn set_invoke_callback(callback: Option<InvokeCallback>) {
    if let Ok(mut guard) = INVOKE_CALLBACK.write() {
        *guard = callback;
    }
}

/// Call the registered invoke callback.
/// Returns Err if no callback is registered or if the callback fails.
pub fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    let guard = INVOKE_CALLBACK
        .read()
        .map_err(|_| "Failed to acquire invoke callback lock".to_string())?;

    let callback = guard.as_ref().ok_or_else(|| {
        "invoke() called outside of execute context - callbacks can only be called during code execution".to_string()
    })?;

    callback(name, args_json)
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

/// The _eryx module definition.
/// This generates a `PyInit__eryx` function that can be registered with Python.
#[pymodule]
#[pyo3(name = "_eryx")]
fn eryx_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_eryx_invoke, m)?)?;
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
            eprintln!("eryx-wasm-runtime: WARNING: Failed to import __main__");
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

/// Execute Python code and capture stdout.
///
/// This is the main entry point for the `execute` WIT export.
/// It runs the provided code in `__main__` and returns captured stdout,
/// or an error message if execution fails.
///
/// # Returns
/// - `Ok(stdout)` - The captured stdout output (may be empty)
/// - `Err(error)` - Error message if execution failed
pub fn execute_python(code: &str) -> Result<String, String> {
    use std::ffi::CString;

    if !is_python_initialized() {
        return Err("Python not initialized".to_string());
    }

    let code_cstr = CString::new(code).map_err(|e| format!("Invalid code string: {e}"))?;

    unsafe {
        // Set up stdout/stderr capture using StringIO
        let capture_setup = c"
import sys as _sys
from io import StringIO as _StringIO
_eryx_stdout = _StringIO()
_eryx_stderr = _StringIO()
_eryx_old_stdout = _sys.stdout
_eryx_old_stderr = _sys.stderr
_sys.stdout = _eryx_stdout
_sys.stderr = _eryx_stderr
";
        if PyRun_SimpleString(capture_setup.as_ptr()) != 0 {
            PyErr_Clear();
            return Err("Failed to set up output capture".to_string());
        }

        // Run the user's code
        let exec_result = PyRun_SimpleString(code_cstr.as_ptr());

        // Restore stdout/stderr and get captured output
        let capture_teardown = c"
_sys.stdout = _eryx_old_stdout
_sys.stderr = _eryx_old_stderr
_eryx_output = _eryx_stdout.getvalue()
_eryx_errors = _eryx_stderr.getvalue()
# Clean up our temporary variables
del _eryx_stdout, _eryx_stderr, _eryx_old_stdout, _eryx_old_stderr
";
        if PyRun_SimpleString(capture_teardown.as_ptr()) != 0 {
            PyErr_Clear();
            // Even if teardown fails, try to continue
        }

        if exec_result != 0 {
            // Execution failed - get the error message
            // First check if there's stderr output
            let stderr_output = get_python_variable_string("_eryx_errors").unwrap_or_default();

            // Also get the actual Python exception if one occurred
            let exception_msg = get_last_error_message();

            // Combine stderr and exception info
            let error = if !stderr_output.is_empty() && exception_msg != "Unknown error" {
                format!("{stderr_output}\n{exception_msg}")
            } else if !stderr_output.is_empty() {
                stderr_output
            } else {
                exception_msg
            };

            // Clean up the temporary variable
            let _ = PyRun_SimpleString(c"del _eryx_output, _eryx_errors".as_ptr());

            return Err(error);
        }

        // Get the captured stdout
        let output = get_python_variable_string("_eryx_output").unwrap_or_default();

        // Clean up the temporary variables
        let _ = PyRun_SimpleString(c"del _eryx_output, _eryx_errors".as_ptr());

        Ok(output)
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

# Callbacks metadata from host
_eryx_callbacks_json = '''{}'''
_eryx_callbacks = _json.loads(_eryx_callbacks_json)

def invoke(name, **kwargs):
    """Invoke a host callback by name with keyword arguments.

    Args:
        name: Name of the callback (e.g., "sleep", "http.get")
        **kwargs: Arguments to pass to the callback

    Returns:
        The callback result (parsed from JSON)

    Example:
        result = invoke("get_time")
        data = invoke("http.get", url="https://example.com")
    """
    # Serialize kwargs to JSON and call the Rust implementation
    args_json = _json.dumps(kwargs)
    result_json = _eryx._eryx_invoke(name, args_json)
    # Parse result JSON (may be empty string for void callbacks)
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

# Helper to create callback wrappers
def _eryx_make_callback(name):
    def callback(**kwargs):
        return invoke(name, **kwargs)
    callback.__name__ = name
    callback.__doc__ = f"Invoke the '{{name}}' callback."
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

    def __call__(self, **kwargs):
        if self._prefix:
            return self._invoke(self._prefix.rstrip('.'), **kwargs)
        raise TypeError("Cannot call root namespace")

class _EryxCallbackLeaf:
    def __init__(self, invoke_fn, name):
        self._invoke = invoke_fn
        self._name = name

    def __call__(self, **kwargs):
        return self._invoke(self._name, **kwargs)

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
