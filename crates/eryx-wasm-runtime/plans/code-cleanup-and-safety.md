# Code Cleanup and Safety Improvements

This plan documents potential improvements to `lib.rs` and `python.rs` focused on:
- Reducing `unsafe` code
- Removing dead code
- Improving maintainability
- Consolidating duplicated logic

## Priority: High (Quick Wins)

### 1. Remove Unused CPython FFI Declarations

**File:** `python.rs`
**Lines saved:** ~80
**Risk:** Low

Since we switched to PyO3 for the `_eryx` module, the following manual C extension structures are no longer used:

```rust
// Remove these (lines 390-471):
pub struct PyMethodDef { ... }
pub type PyCFunction = ...;
pub type PyCFunctionWithKeywords = ...;
pub const METH_VARARGS: c_int = 0x0001;
pub const METH_KEYWORDS: c_int = 0x0002;
pub const METH_NOARGS: c_int = 0x0004;
pub const METH_O: c_int = 0x0008;
pub const fn PyMethodDef_SENTINEL() -> PyMethodDef { ... }
pub struct PyModuleDef { ... }
pub struct PyModuleDef_Base { ... }
pub struct PyModuleDef_Slot { ... }
pub const PYTHON_API_VERSION: c_int = 1013;
pub const fn PyModuleDef_HEAD_INIT() -> PyModuleDef_Base { ... }
```

Also remove the unused `PyImport_AppendInittab` and `PyModule_Create2` declarations since PyO3 handles module registration.

### 2. Remove Debug `eprintln!` Statements

**Files:** `lib.rs`, `python.rs`
**Lines saved:** ~20
**Risk:** Low

Remove or make conditional:

```rust
// lib.rs - remove these:
eprintln!("eryx-wasm-runtime: initialize called");
eprintln!("eryx-wasm-runtime: export_start for func {}", func.index());
eprintln!("eryx-wasm-runtime: export_call for func {}", func.index());
eprintln!("eryx-wasm-runtime: execute called with code: {code}");
// ... etc

// Option A: Remove entirely
// Option B: Feature flag
#[cfg(feature = "debug-logging")]
eprintln!("...");
```

### 3. Refactor Duplicated Export Handlers

**File:** `lib.rs`
**Lines saved:** ~80
**Risk:** Low

`export_call` and `export_async_start` have nearly identical match statements:

```rust
// Before: Two copies of the same logic

// After: Single implementation
fn handle_export(wit: Wit, func_index: usize, cx: &mut EryxCall) -> bool {
    match func_index {
        EXPORT_EXECUTE => {
            let code = cx.pop_string().to_string();
            ensure_callbacks_initialized(wit);
            let result = with_wit(wit, || python::execute_python(&code));
            match result {
                Ok(output) => {
                    cx.push_string(output);
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
            true // needs task_return for async
        }
        // ... other exports
    }
}

impl Interpreter for EryxInterpreter {
    fn export_call(wit: Wit, func: ExportFunction, cx: &mut Self::CallCx<'_>) {
        handle_export(wit, func.index(), cx);
    }

    fn export_async_start(wit: Wit, func: ExportFunction, mut cx: Box<Self::CallCx<'static>>) -> u32 {
        let needs_return = handle_export(wit, func.index(), &mut cx);
        if needs_return {
            if let Some(task_return) = func.task_return() {
                unsafe {
                    task_return(Box::into_raw(cx).cast());
                }
                return 0;
            }
        }
        drop(cx);
        0
    }
}
```

---

## Priority: Medium

### 4. Use `pyo3::ffi` Instead of Manual Declarations

**File:** `python.rs`
**Lines saved:** ~300
**Risk:** Medium (requires testing)

Since we already depend on PyO3, we could use its FFI module:

```rust
// Before: Manual declarations (lines 42-384)
unsafe extern "C" {
    pub fn Py_Initialize();
    pub fn Py_InitializeEx(initsigs: c_int);
    pub fn PyRun_SimpleString(command: *const c_char) -> c_int;
    // ... 100+ more declarations
}

// After: Use pyo3::ffi
use pyo3::ffi::{
    Py_Initialize, Py_InitializeEx, PyRun_SimpleString,
    PyObject, PyErr_Occurred, PyErr_Clear, PyErr_Fetch,
    PyImport_AddModule, PyModule_GetDict,
    PyDict_GetItemString, PyObject_Str,
    PyUnicode_AsUTF8, Py_IncRef, Py_DecRef,
    PyBytes_FromStringAndSize, PyBytes_AsString, PyBytes_Size,
    // etc.
};
```

**Caveats:**
- Need to verify pyo3::ffi exports all functions we need
- May need `pyo3 = { features = ["abi3-py312", "extension-module"] }`
- Test thoroughly in WASM environment

### 5. Fix Memory Leak in `pop_string`

**File:** `lib.rs`
**Risk:** Medium

Current implementation leaks memory:

```rust
fn pop_string(&mut self) -> &str {
    match self.stack.pop() {
        Some(Value::String(s)) => {
            let leaked: &'static str = Box::leak(s.into_boxed_str());
            leaked
        }
        // ...
    }
}
```

Options:

**Option A: Use the `deferred` mechanism (already exists but unused)**
```rust
fn pop_string(&mut self) -> &str {
    match self.stack.pop() {
        Some(Value::String(s)) => {
            let boxed = s.into_boxed_str();
            let ptr = Box::into_raw(boxed);
            let layout = Layout::for_value(unsafe { &*ptr });
            self.deferred.push((ptr as *mut u8, layout));
            unsafe { &*ptr }
        }
        // ...
    }
}

// Add cleanup method
impl EryxCall {
    fn cleanup(&mut self) {
        for (ptr, layout) in self.deferred.drain(..) {
            unsafe { std::alloc::dealloc(ptr, layout); }
        }
    }
}
```

**Option B: Store strings in a separate arena**
```rust
pub struct EryxCall {
    stack: Vec<Value>,
    string_arena: Vec<String>,  // Keep strings alive
    deferred: Vec<(*mut u8, Layout)>,
}

fn pop_string(&mut self) -> &str {
    match self.stack.pop() {
        Some(Value::String(s)) => {
            self.string_arena.push(s);
            self.string_arena.last().unwrap().as_str()
        }
        // ...
    }
}
```

### 6. Simplify INVOKE_CALLBACK (Single-Threaded)

**File:** `python.rs`
**Risk:** Low

WASM is single-threaded, so `RwLock` is overkill:

```rust
// Before
static INVOKE_CALLBACK: std::sync::RwLock<Option<InvokeCallback>> =
    std::sync::RwLock::new(None);

pub fn set_invoke_callback(callback: Option<InvokeCallback>) {
    if let Ok(mut guard) = INVOKE_CALLBACK.write() {
        *guard = callback;
    }
}

pub fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    let guard = INVOKE_CALLBACK.read()
        .map_err(|_| "Failed to acquire invoke callback lock")?;
    // ...
}

// After: Thread-local (simpler, no locking)
use std::cell::RefCell;

thread_local! {
    static INVOKE_CALLBACK: RefCell<Option<InvokeCallback>> = RefCell::new(None);
}

pub fn set_invoke_callback(callback: Option<InvokeCallback>) {
    INVOKE_CALLBACK.with(|cell| *cell.borrow_mut() = callback);
}

pub fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    INVOKE_CALLBACK.with(|cell| {
        let callback = cell.borrow();
        let callback = callback.as_ref().ok_or_else(||
            "invoke() called outside of execute context")?;
        callback(name, args_json)
    })
}
```

---

## Priority: Low (Nice to Have)

### 7. Extract Python Code to Constants

**File:** `python.rs`
**Risk:** Low (readability improvement)

Move large Python code blocks to module-level constants:

```rust
/// Python code to set up stdout/stderr capture
const CAPTURE_SETUP_PY: &[u8] = b"
import sys as _sys
from io import StringIO as _StringIO
_eryx_stdout = _StringIO()
_eryx_stderr = _StringIO()
_eryx_old_stdout = _sys.stdout
_eryx_old_stderr = _sys.stderr
_sys.stdout = _eryx_stdout
_sys.stderr = _eryx_stderr
\0";

/// Python code to tear down capture and get output
const CAPTURE_TEARDOWN_PY: &[u8] = b"
_sys.stdout = _eryx_old_stdout
_sys.stderr = _eryx_old_stderr
_eryx_output = _eryx_stdout.getvalue()
_eryx_errors = _eryx_stderr.getvalue()
del _eryx_stdout, _eryx_stderr, _eryx_old_stdout, _eryx_old_stderr
\0";

// Usage:
PyRun_SimpleString(CAPTURE_SETUP_PY.as_ptr().cast());
```

### 8. Create Safe Wrapper Types

**File:** `python.rs`
**Risk:** Medium

Wrap raw Python objects in RAII guards:

```rust
/// RAII wrapper for Python object references
struct PyRef(*mut PyObject);

impl PyRef {
    /// Create from a new reference (takes ownership)
    fn new(ptr: *mut PyObject) -> Option<Self> {
        if ptr.is_null() { None } else { Some(Self(ptr)) }
    }

    /// Create from a borrowed reference (increments refcount)
    fn borrowed(ptr: *mut PyObject) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            unsafe { Py_IncRef(ptr); }
            Some(Self(ptr))
        }
    }

    fn as_ptr(&self) -> *mut PyObject {
        self.0
    }
}

impl Drop for PyRef {
    fn drop(&mut self) {
        unsafe { Py_DecRef(self.0); }
    }
}

// Usage:
fn get_error_message() -> String {
    unsafe {
        let str_obj = PyRef::new(PyObject_Str(pvalue))?;
        let utf8 = PyUnicode_AsUTF8(str_obj.as_ptr());
        // str_obj automatically decremented when dropped
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}
```

### 9. Consolidate Unsafe Blocks

**File:** `python.rs`
**Risk:** Low

Instead of many small unsafe blocks, create clearly-documented unsafe helper functions:

```rust
/// Execute Python code and return result.
///
/// # Safety
/// - Python must be initialized
/// - `code` must be a valid null-terminated C string
unsafe fn py_run_simple_string_unchecked(code: *const c_char) -> Result<(), String> {
    let result = PyRun_SimpleString(code);
    if result == 0 {
        Ok(())
    } else {
        Err(get_last_error_message())
    }
}

// Public safe wrapper
pub fn run_simple_string(code: &str) -> Result<(), String> {
    let code_cstr = CString::new(code)
        .map_err(|e| format!("Invalid code string: {e}"))?;
    unsafe { py_run_simple_string_unchecked(code_cstr.as_ptr()) }
}
```

---

## Summary

| Task | Priority | Lines Changed | Unsafe Reduction |
|------|----------|---------------|------------------|
| Remove unused PyMethodDef/PyModuleDef | High | -80 | None |
| Remove debug eprintln! | High | -20 | None |
| Refactor duplicated export handlers | High | -80 | Minor |
| Use pyo3::ffi | Medium | -300 | Minor |
| Fix pop_string memory leak | Medium | +20 | None |
| Simplify INVOKE_CALLBACK | Medium | -10 | None |
| Extract Python to constants | Low | 0 | None |
| Create PyRef wrapper | Low | +50 | Moderate |
| Consolidate unsafe blocks | Low | 0 | Clarity |

**Recommended order:**
1. Remove unused code (quick, low risk)
2. Remove debug output
3. Refactor duplicated handlers
4. Fix memory leak
5. Simplify INVOKE_CALLBACK
6. Use pyo3::ffi (if testing confirms compatibility)
