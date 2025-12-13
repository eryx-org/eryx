# Eryx WASM Runtime Implementation Plan

This document describes the chosen approach for native Python extension support and the detailed next steps for implementation.

## Prerequisites / Background

Before diving in, read **[native-extensions-research.md](./native-extensions-research.md)** which explains:
- Why we need a custom runtime (avoiding componentize-py recompilation)
- The three approaches explored and why this one was chosen
- How wit-dylib and the interpreter interface work
- The build process for WASI dynamic libraries

**Key reference:** The [componentize-py runtime](https://github.com/bytecodealliance/componentize-py/tree/main/runtime) is the canonical implementation of a wit-dylib interpreter that calls Python. Study it when stuck.

## Chosen Approach: Custom wit-dylib Runtime

We've implemented a custom WASM runtime (`eryx-wasm-runtime`) that replaces componentize-py's `libcomponentize_py_runtime.so`. This approach:

1. Hardcodes the eryx sandbox exports (no generic dispatch needed)
2. Implements the wit-dylib interpreter interface directly in Rust
3. Will call CPython via FFI for actual Python execution
4. Allows native extensions to be linked at runtime without recompilation

## The WIT Interface

The eryx sandbox exports are defined in `crates/eryx-runtime/runtime.wit`:

```wit
package eryx:sandbox;

world sandbox {
    /// Call a host-provided function by name with JSON arguments
    import invoke: async func(name: string, args: string) -> result<string, string>;

    /// List available callback functions
    import list-callbacks: func() -> list<string>;

    /// Report a trace/span for observability
    import report-trace: func(trace: string);

    /// Execute Python code and return stdout
    export execute: async func(code: string) -> result<string, string>;

    /// Snapshot the current Python globals state
    export snapshot-state: async func() -> result<list<u8>, string>;

    /// Restore Python globals from a snapshot
    export restore-state: async func(state: list<u8>) -> result<_, string>;

    /// Clear all Python globals
    export clear-state: async func();
}
```

Note: All exports are `async` which requires special handling (see "Async Export Handling" below).

## Current Implementation Status

### What's Already Built

The `crates/eryx-wasm-runtime/` crate contains a **working stub implementation**:

**`src/lib.rs`** - The main implementation with:
- `EryxCall` struct - A call context that holds a stack of values for passing data between wit-dylib and our code
- `EryxInterpreter` struct - Implements the `Interpreter` trait from wit-dylib-ffi
- Export handlers for `execute`, `snapshot-state`, `restore-state`, `clear-state`
- Currently returns stub values (e.g., `execute("code")` returns `"executed: code"`)

**Key code to understand:**

```rust
// The call context - holds values being passed to/from exports
pub struct EryxCall {
    stack: Vec<Value>,           // Stack of WIT values
    deferred: Vec<(*mut u8, Layout)>,  // Deferred deallocations
}

// The interpreter - handles export dispatch
impl Interpreter for EryxInterpreter {
    type CallCx<'a> = EryxCall;

    fn initialize(_wit: Wit) { /* Called once at startup */ }

    fn export_start<'a>(_wit: Wit, func: ExportFunction) -> Box<Self::CallCx<'a>> {
        // Create a new call context for this export invocation
        Box::new(EryxCall::new())
    }

    fn export_async_start(
        _wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        // Handle async exports - pop args, do work, push results
        match func.index() {
            EXPORT_EXECUTE => {
                let code = cx.pop_string().to_string();
                // TODO: Actually run Python here
                let result = format!("executed: {code}");
                cx.push_string(result);
                cx.stack.push(Value::ResultDiscriminant(true));
            }
            // ... other exports
        }

        // CRITICAL: Call task_return to signal completion
        if let Some(task_return) = func.task_return() {
            unsafe { task_return(Box::into_raw(cx).cast()); }
        }
        0  // Return 0 for synchronous completion
    }
}
```

**Build infrastructure:**
- `build.sh` - Compiles Rust to staticlib, links with WASI SDK Clang to create `.so`
- `clock_stubs.c` - Provides `_CLOCK_PROCESS_CPUTIME_ID` and `_CLOCK_THREAD_CPUTIME_ID` symbols

**Tests:**
- `tests/link_test.rs` - Verifies the `.so` can be linked into a component
- `tests/runtime_test.rs` - End-to-end test that instantiates with wasmtime and calls `execute`

### What's NOT Built Yet

1. **CPython FFI** - No Python calls yet, just stubs
2. **Integration with eryx-runtime** - This branch has the standalone crate only. The `feat/late-linking-exploration` branch has full integration with the linker.

## Development Workflow

### Building the Runtime

```bash
cd crates/eryx-wasm-runtime

# Install WASI SDK 27+ if not already installed
# Download from https://github.com/aspect/wasi-sdk/releases
# Extract to /path/to/wasi-sdk or set WASI_SDK_PATH

# Build the .so file
./build.sh

# Output: target/liberyx_runtime.so
```

### Running Tests

```bash
# Run the link test (verifies .so structure)
cargo test -p eryx-wasm-runtime --test link_test

# Run the runtime test (instantiates with wasmtime)
cargo test -p eryx-wasm-runtime --test runtime_test -- --nocapture
```

### Debugging

The runtime prints debug info to stderr:
```
eryx-wasm-runtime: initialize called
eryx-wasm-runtime: export_start for func 0
eryx-wasm-runtime: export_async_start for func 0
eryx-wasm-runtime: async execute called with code: print(1+1)
eryx-wasm-runtime: calling task_return
```

To see more detail, enable debug printing in wit-dylib-ffi by changing `debug_println!` in their code.

## Async Export Handling

All eryx exports are async. This is important because:

1. **wasmtime requires async config:**
   ```rust
   let mut config = Config::new();
   config.async_support(true);
   config.wasm_component_model(true);
   config.wasm_component_model_async(true);  // Required!
   ```

2. **Must call `task_return` to complete:**
   ```rust
   // After pushing results to the call context:
   if let Some(task_return) = func.task_return() {
       unsafe {
           let cx_ptr = Box::into_raw(cx);
           task_return(cx_ptr.cast());
           // cx is now consumed - don't use it!
       }
   }
   ```

3. **Return value indicates completion status:**
   - `0` = completed synchronously (what we do now)
   - Non-zero = task handle for true async (not implemented yet)

## Next Steps: CPython FFI Integration

### Step 1: Add CPython Headers and Bindings

**Goal:** Generate Rust bindings for CPython's C API.

**Tasks:**
1. Add `pyo3-ffi` or generate custom bindings with `bindgen`
2. Define the minimal CPython API surface needed:
   - `Py_Initialize()` / `Py_InitializeEx()`
   - `PyRun_SimpleString()`
   - `PyRun_String()` with compilation flags
   - `PyErr_Occurred()` / `PyErr_Fetch()` / `PyErr_Clear()`
   - `PyObject_Str()` for converting results
   - `Py_DECREF()` / `Py_INCREF()` for reference counting
   - `PyDict_GetItemString()` / `PyDict_SetItemString()` for accessing globals
   - `PyBytes_AsString()` / `PyBytes_FromStringAndSize()` for byte data

**Files to create/modify:**
- `crates/eryx-wasm-runtime/Cargo.toml` - Add pyo3-ffi or bindgen
- `crates/eryx-wasm-runtime/src/python.rs` - New module for Python FFI

**Considerations:**
- The Python symbols come from `libpython3.14.so` which is linked at component link time
- We declare the symbols as `extern "C"` and they resolve during linking
- No need to dlopen libpython - it's statically linked into the component
- Look at componentize-py's `runtime/src/lib.rs` for reference

**Example FFI declaration:**
```rust
// src/python.rs
use std::ffi::c_char;

#[link(name = "python3.14")]
extern "C" {
    pub fn Py_InitializeEx(initsigs: i32);
    pub fn Py_IsInitialized() -> i32;
    pub fn PyRun_SimpleString(command: *const c_char) -> i32;
    pub fn PyErr_Occurred() -> *mut PyObject;
    pub fn PyErr_Clear();
    // ... etc
}

#[repr(C)]
pub struct PyObject {
    _private: [u8; 0],
}
```

### Step 2: Implement Python Interpreter Management

**Goal:** Initialize and manage the Python interpreter lifecycle.

**Tasks:**
1. Initialize Python once during `wit_dylib_initialize()`
2. Set up sys.path to include bundled stdlib and site-packages
3. Configure stdout/stderr capture for result collection
4. Handle interpreter cleanup in component teardown

**Implementation sketch:**
```rust
// In src/lib.rs or src/python.rs

use std::sync::atomic::{AtomicBool, Ordering};

static PYTHON_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn initialize_python() {
    if PYTHON_INITIALIZED.swap(true, Ordering::SeqCst) {
        return; // Already initialized
    }

    unsafe {
        // Don't register signal handlers (we're in WASM)
        Py_InitializeEx(0);

        // Set up sys.path for stdlib
        let setup_code = c_str!(r#"
import sys
sys.path.insert(0, '/python-stdlib')
sys.path.insert(0, '/site-packages')
"#);
        PyRun_SimpleString(setup_code);
    }
}

// Call from Interpreter::initialize()
impl Interpreter for EryxInterpreter {
    fn initialize(_wit: Wit) {
        initialize_python();
    }
    // ...
}
```

### Step 3: Implement `execute` Export

**Goal:** Actually run Python code and return results.

**Tasks:**
1. Capture stdout/stderr during execution
2. Handle Python exceptions and convert to error results
3. Return the captured output or error message

**Implementation sketch:**
```rust
fn execute_python(code: &str) -> Result<String, String> {
    unsafe {
        // Set up output capture
        let capture_setup = c_str!(r#"
import sys
from io import StringIO
_eryx_stdout = StringIO()
_eryx_stderr = StringIO()
_eryx_old_stdout = sys.stdout
_eryx_old_stderr = sys.stderr
sys.stdout = _eryx_stdout
sys.stderr = _eryx_stderr
"#);
        PyRun_SimpleString(capture_setup);

        // Run the user code
        let code_cstr = CString::new(code).map_err(|e| e.to_string())?;
        let result = PyRun_SimpleString(code_cstr.as_ptr());

        // Restore stdout/stderr and get captured output
        let capture_teardown = c_str!(r#"
sys.stdout = _eryx_old_stdout
sys.stderr = _eryx_old_stderr
_eryx_output = _eryx_stdout.getvalue()
_eryx_errors = _eryx_stderr.getvalue()
"#);
        PyRun_SimpleString(capture_teardown);

        if result != 0 {
            // Execution failed - get error
            let errors = get_python_global("_eryx_errors")?;
            return Err(errors);
        }

        let output = get_python_global("_eryx_output")?;
        Ok(output)
    }
}

// Helper to get a Python global variable as a Rust String
fn get_python_global(name: &str) -> Result<String, String> {
    unsafe {
        let main_module = PyImport_AddModule(c_str!("__main__"));
        let globals = PyModule_GetDict(main_module);
        let name_cstr = CString::new(name).unwrap();
        let value = PyDict_GetItemString(globals, name_cstr.as_ptr());

        if value.is_null() {
            return Err(format!("Variable {} not found", name));
        }

        // Convert PyObject to Rust String
        let str_obj = PyObject_Str(value);
        let str_ptr = PyUnicode_AsUTF8(str_obj);
        let result = CStr::from_ptr(str_ptr).to_string_lossy().into_owned();
        Py_DECREF(str_obj);

        Ok(result)
    }
}
```

### Step 4: Implement State Management Exports

**Goal:** Support snapshot-state, restore-state, and clear-state.

**Tasks:**
1. `snapshot-state`: Pickle `__main__.__dict__` and return bytes
2. `restore-state`: Unpickle bytes into `__main__.__dict__`
3. `clear-state`: Reset `__main__.__dict__` to empty

**Implementation sketch:**
```rust
fn snapshot_state() -> Result<Vec<u8>, String> {
    unsafe {
        let snapshot_code = c_str!(r#"
import pickle
import __main__
# Filter out non-picklable builtins
_eryx_state = {k: v for k, v in __main__.__dict__.items()
               if not k.startswith('_eryx_') and k not in ('__builtins__', '__loader__', '__spec__')}
_eryx_state_bytes = pickle.dumps(_eryx_state)
"#);
        let result = PyRun_SimpleString(snapshot_code);
        if result != 0 {
            return Err("Failed to snapshot state".into());
        }

        // Get _eryx_state_bytes as Vec<u8>
        let bytes = get_python_global_bytes("_eryx_state_bytes")?;
        Ok(bytes)
    }
}

fn restore_state(data: &[u8]) -> Result<(), String> {
    unsafe {
        // Set the bytes in Python
        set_python_global_bytes("_eryx_restore_data", data)?;

        let restore_code = c_str!(r#"
import pickle
import __main__
_eryx_restored = pickle.loads(_eryx_restore_data)
__main__.__dict__.update(_eryx_restored)
del _eryx_restore_data
"#);
        let result = PyRun_SimpleString(restore_code);
        if result != 0 {
            return Err("Failed to restore state".into());
        }
        Ok(())
    }
}

fn clear_state() {
    unsafe {
        let clear_code = c_str!(r#"
import __main__
_eryx_keep = {'__builtins__', '__name__', '__doc__', '__package__', '__loader__', '__spec__'}
for _k in list(__main__.__dict__.keys()):
    if _k not in _eryx_keep and not _k.startswith('_eryx_'):
        del __main__.__dict__[_k]
"#);
        PyRun_SimpleString(clear_code);
    }
}
```

### Step 5: Handle Import Callbacks

**Goal:** Support the `invoke` import for calling host functions from Python.

The sandbox WIT has an `invoke` import that Python code uses to call host-provided functions. The flow is:

```
Python code calls invoke("func", "args")
    → Our C extension function eryx_invoke()
    → Rust calls wit-dylib import mechanism
    → Host receives the call and returns result
    → Result flows back to Python
```

**How to call WIT imports from Rust:**

The `Wit` handle passed to `initialize()` provides access to imports:

```rust
use std::cell::RefCell;

// Store the Wit handle for later use
thread_local! {
    static WIT: RefCell<Option<Wit>> = RefCell::new(None);
}

impl Interpreter for EryxInterpreter {
    fn initialize(wit: Wit) {
        WIT.with(|w| *w.borrow_mut() = Some(wit));
        initialize_python();
    }
}

// Call an import from Rust
fn call_invoke(name: &str, args: &str) -> Result<String, String> {
    WIT.with(|w| {
        let wit = w.borrow();
        let wit = wit.as_ref().expect("WIT not initialized");

        // Get the import function
        let func = wit.unwrap_import(None, "invoke");  // None = no interface prefix

        // Create a call context and push arguments
        let mut cx = EryxCall::new();
        cx.push_string(name.to_string());
        cx.push_string(args.to_string());

        // Call the import (this is synchronous for now)
        func.call_import_sync(&mut cx);

        // Pop the result
        let is_err = cx.pop_result(...);  // 0 = ok, 1 = err
        let result = cx.pop_string().to_string();

        if is_err == 0 {
            Ok(result)
        } else {
            Err(result)
        }
    })
}
```

**Exposing to Python:**

You'll need to create a Python C extension module that Python code can import:

```rust
// This gets compiled into the runtime and is callable from Python
#[no_mangle]
pub extern "C" fn PyInit_eryx() -> *mut PyObject {
    // Create a module with an "invoke" function
    // See Python C API docs for PyModule_Create
}

#[no_mangle]
pub extern "C" fn eryx_invoke_impl(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    // Parse args (name, args_json)
    // Call our Rust call_invoke()
    // Convert result back to PyObject
}
```

Alternatively, inject a pure-Python wrapper at startup:
```rust
let inject_code = c_str!(r#"
import ctypes

# Get the C function pointer
_eryx_invoke_ptr = ... # somehow expose this

def invoke(name, args):
    # Call the C function
    result = _eryx_invoke_ptr(name.encode(), args.encode())
    return result.decode()
"#);
```

### Step 6: Build System Updates

**Goal:** Build liberyx_runtime.so with Python support.

**Tasks:**
1. Update build.sh to ensure libpython symbols are available
2. Add any additional stub symbols needed
3. Test the complete build pipeline

**Updated build.sh considerations:**
```bash
# The Python symbols come from libpython3.14.so at link time
# We don't link against it during build - symbols resolve when
# wit-component::Linker combines all the .so files

# Our runtime just needs to declare the symbols as extern "C"
# and they'll resolve at component link time
```

**Additional stubs that might be needed:**
```c
// clock_stubs.c - expand if needed
int _CLOCK_PROCESS_CPUTIME_ID = 2;
int _CLOCK_THREAD_CPUTIME_ID = 3;

// Add more if you get undefined symbol errors during linking
```

### Step 7: Testing Strategy

**Unit tests** (in `tests/python_test.rs`):
```rust
#[test]
fn test_python_init() {
    // Python initializes without crashing
}

#[test]
fn test_execute_simple() {
    // execute("print(1+1)") returns "2\n"
}

#[test]
fn test_execute_import() {
    // execute("import sys; print(sys.version)") works
}

#[test]
fn test_execute_error() {
    // execute("raise ValueError('test')") returns Err with traceback
}

#[test]
fn test_state_roundtrip() {
    // execute("x = 42")
    // snapshot = snapshot_state()
    // clear_state()
    // execute("print(x)") -> error
    // restore_state(snapshot)
    // execute("print(x)") -> "42\n"
}
```

**Integration tests:**
- Link complete component with wasmtime
- Call execute with various Python code
- Verify stdout capture works
- Test native extension imports (numpy, etc.)

### Step 8: Performance Optimization

**Areas to optimize:**
1. **Python initialization** - Consider pre-initialization (capture memory state after init)
2. **String conversion** - Minimize copies between Rust/Python
3. **Output capture** - Use more efficient mechanism than StringIO (maybe a custom file object)
4. **State serialization** - Consider alternatives to pickle for large state (marshal, custom format)

## File Structure After Implementation

```
crates/eryx-wasm-runtime/
├── Cargo.toml
├── build.sh
├── clock_stubs.c
├── src/
│   ├── lib.rs           # Main interpreter implementation
│   ├── python.rs        # CPython FFI bindings and helpers
│   ├── state.rs         # State management (snapshot/restore/clear)
│   └── invoke.rs        # Host function callbacks
├── tests/
│   ├── link_test.rs     # Verify .so structure
│   ├── runtime_test.rs  # Wasmtime instantiation
│   └── python_test.rs   # Python execution tests
└── target/
    └── liberyx_runtime.so
```

## Dependencies

```toml
[dependencies]
wit-dylib-ffi = { ... }

# For CPython FFI (choose one approach)
# Option A: Use pyo3-ffi for pre-generated bindings
pyo3-ffi = { version = "0.22", features = ["abi3-py314"] }

# Option B: Generate custom minimal bindings
# (Use bindgen in build.rs with Python headers)
```

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| CPython initialization fails in WASM | Test early with minimal init, check WASI compatibility. componentize-py already does this, so it should work. |
| Symbol resolution issues | Carefully track which symbols come from which library. Use `wasm-objdump -x` to inspect imports/exports. |
| Memory management bugs | Use RAII patterns, careful with Python refcounting. Consider using pyo3's safe wrappers where possible. |
| Performance regression | Benchmark against componentize-py baseline early. |
| Native extension compatibility | Test with real extensions (numpy, pydantic) early. |
| `task_return` not called | Always call it! Missing this causes "async-lifted export failed to produce a result" error. |

## Success Criteria

1. `execute("print(1+1)")` returns `"2\n"` (not stub value)
2. `execute("import sys; print(sys.version)")` works
3. State snapshot/restore round-trips correctly
4. Native extensions (numpy) can be imported and used
5. Performance is comparable to componentize-py approach
6. Component size is reasonable (~30-35MB with full Python)

## Timeline Estimate

| Phase | Effort |
|-------|--------|
| Step 1-2: CPython FFI setup | 1-2 days |
| Step 3: Execute implementation | 1-2 days |
| Step 4: State management | 1 day |
| Step 5: Import callbacks | 1-2 days |
| Step 6-7: Build & testing | 1-2 days |
| Step 8: Optimization | 1-2 days |
| **Total** | **7-12 days** |

## References

- [CPython C API](https://docs.python.org/3/c-api/index.html) - Official C API docs
- [pyo3-ffi](https://docs.rs/pyo3-ffi/latest/pyo3_ffi/) - Low-level Python FFI bindings for Rust
- [componentize-py runtime](https://github.com/bytecodealliance/componentize-py/tree/main/runtime) - **Key reference!** Study this implementation
- [wit-dylib-ffi](https://github.com/dicej/wasm-tools/tree/main/crates/wit-dylib/ffi) - The interpreter interface we implement
- [native-extensions-research.md](./native-extensions-research.md) - Background on why this approach was chosen
- [eryx-wasm-runtime](../crates/eryx-wasm-runtime/) - Current stub implementation
