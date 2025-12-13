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

use std::ffi::{c_char, c_int, c_long};
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
// Python interpreter state
// =============================================================================

/// Track whether we've initialized Python.
static PYTHON_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize Python interpreter.
///
/// This should be called once during `wit_dylib_initialize`.
/// Subsequent calls are no-ops.
///
/// Sets up:
/// - Python interpreter (without signal handlers - not useful in WASM)
/// - sys.path to include /python-stdlib and /site-packages
pub fn initialize_python() {
    if PYTHON_INITIALIZED.swap(true, Ordering::SeqCst) {
        // Already initialized
        return;
    }

    unsafe {
        // Initialize Python without registering signal handlers.
        // Signal handlers don't make sense in a WASM sandbox.
        Py_InitializeEx(0);

        // Set up sys.path to include bundled stdlib and site-packages.
        // These paths match where componentize-py mounts the Python files
        // in the WASM filesystem.
        let setup_code = c"
import sys

# Clear default paths and set up our sandbox paths
sys.path.clear()
sys.path.append('/python-stdlib')
sys.path.append('/site-packages')

# Also ensure __main__ module exists for code execution
import __main__
";
        let result = PyRun_SimpleString(setup_code.as_ptr());
        if result != 0 {
            // If this fails, Python is in a bad state - not much we can do
            eprintln!("eryx-wasm-runtime: WARNING: Failed to set up sys.path");
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

#[cfg(test)]
mod tests {
    // Tests will be added when we can actually run Python
    // For now, just verify the module compiles
}
