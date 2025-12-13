# Eryx WASM Runtime Implementation Plan

This document describes the chosen approach for native Python extension support and the detailed next steps for implementation.

## Chosen Approach: Custom wit-dylib Runtime

We've implemented a custom WASM runtime (`eryx-wasm-runtime`) that replaces componentize-py's `libcomponentize_py_runtime.so`. This approach:

1. Hardcodes the eryx sandbox exports (no generic dispatch needed)
2. Implements the wit-dylib interpreter interface directly in Rust
3. Will call CPython via FFI for actual Python execution
4. Allows native extensions to be linked at runtime without recompilation

## Current Implementation Status

### Completed

1. **eryx-wasm-runtime crate** (`crates/eryx-wasm-runtime/`)
   - Implements `Interpreter` trait from wit-dylib-ffi
   - Handles all WIT value types (strings, results, lists, etc.)
   - Properly handles async exports with `task_return`
   - Builds to `liberyx_runtime.so` with proper `@dylink.0` metadata

2. **Build infrastructure**
   - `build.sh` - Compiles Rust staticlib and links with Clang
   - `clock_stubs.c` - Provides missing libc symbols
   - Uses nightly Rust with `-Z build-std` for PIC support

3. **Integration with eryx-runtime**
   - `liberyx_runtime.so.zst` embedded in libs/
   - `liberyx_bindings.so.zst` generated at build time
   - `link_with_eryx_runtime()` function for linking
   - All tests passing

4. **Test coverage**
   - `tests/link_test.rs` - Verifies wit-component linking
   - `tests/runtime_test.rs` - End-to-end wasmtime instantiation
   - `tests/linker_tests.rs` - Integration with eryx-runtime

### Current Limitations

The runtime currently returns stub values:
```rust
// execute("print(1+1)") returns "executed: print(1+1)"
let result = format!("executed: {code}");
```

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

**Files to modify:**
- `crates/eryx-wasm-runtime/Cargo.toml` - Add pyo3-ffi or bindgen
- `crates/eryx-wasm-runtime/src/python.rs` - New module for Python FFI

**Considerations:**
- The Python symbols come from `libpython3.14.so` which is linked at component link time
- We declare the symbols as `extern "C"` and they resolve during linking
- No need to dlopen libpython - it's statically linked into the component

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

static PYTHON_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn initialize_python() {
    if PYTHON_INITIALIZED.swap(true, Ordering::SeqCst) {
        return; // Already initialized
    }

    unsafe {
        // Don't register signal handlers (we're in WASM)
        Py_InitializeEx(0);

        // Set up sys.path for stdlib
        let setup_code = r#"
import sys
sys.path.insert(0, '/python-stdlib')
sys.path.insert(0, '/site-packages')
"#;
        PyRun_SimpleString(setup_code.as_ptr() as *const c_char);
    }
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
        let capture_setup = r#"
import sys
from io import StringIO
_stdout_capture = StringIO()
_stderr_capture = StringIO()
_old_stdout = sys.stdout
_old_stderr = sys.stderr
sys.stdout = _stdout_capture
sys.stderr = _stderr_capture
"#;
        PyRun_SimpleString(capture_setup.as_ptr() as *const c_char);

        // Run the user code
        let code_cstr = CString::new(code).unwrap();
        let result = PyRun_SimpleString(code_cstr.as_ptr());

        // Restore and get output
        let capture_teardown = r#"
sys.stdout = _old_stdout
sys.stderr = _old_stderr
_output = _stdout_capture.getvalue()
_errors = _stderr_capture.getvalue()
"#;
        PyRun_SimpleString(capture_teardown.as_ptr() as *const c_char);

        if result != 0 {
            // Execution failed - get error
            let errors = get_python_variable("_errors");
            return Err(errors);
        }

        let output = get_python_variable("_output");
        Ok(output)
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
    let pickle_code = r#"
import pickle
import __main__
_state_bytes = pickle.dumps(dict(__main__.__dict__))
"#;
    // Run and extract _state_bytes
}

fn restore_state(data: &[u8]) -> Result<(), String> {
    // Write data to a temp location Python can read
    // Run: __main__.__dict__.update(pickle.loads(_state_bytes))
}

fn clear_state() {
    let clear_code = r#"
import __main__
# Keep only builtins
_keep = {'__builtins__', '__name__', '__doc__', '__package__', '__loader__', '__spec__'}
for _k in list(__main__.__dict__.keys()):
    if _k not in _keep:
        del __main__.__dict__[_k]
"#;
    // Run clear code
}
```

### Step 5: Handle Import Callbacks

**Goal:** Support the `invoke` import for calling host functions.

The sandbox WIT has an `invoke` import that Python code uses to call host-provided functions. We need to:

1. Register a Python module that wraps `invoke`
2. When Python calls `invoke("func_name", "args_json")`, call the WIT import
3. Return the result back to Python

**Implementation sketch:**
```rust
// Called from Python via a C extension module
#[no_mangle]
pub extern "C" fn eryx_invoke(name: *const c_char, args: *const c_char) -> *mut c_char {
    let name = unsafe { CStr::from_ptr(name) }.to_str().unwrap();
    let args = unsafe { CStr::from_ptr(args) }.to_str().unwrap();

    // Call the WIT import
    // This requires storing the Wit handle and using import_call
    let result = call_wit_import("invoke", &[name, args]);

    // Return result to Python
    CString::new(result).unwrap().into_raw()
}
```

### Step 6: Build System Updates

**Goal:** Build liberyx_runtime.so with Python support.

**Tasks:**
1. Update build.sh to link against libpython symbols
2. Ensure all required Python/libc symbols are available
3. Test the complete build pipeline

**Build considerations:**
- libpython3.14.so provides Python symbols
- Our runtime declares them as extern and they resolve at link time
- May need additional stub symbols for missing libc functions

### Step 7: Testing Strategy

**Unit tests:**
- `test_python_init` - Python initializes without crashing
- `test_execute_simple` - Basic print/arithmetic works
- `test_execute_import` - Can import stdlib modules
- `test_execute_error` - Exceptions return as errors
- `test_state_roundtrip` - snapshot/restore preserves state

**Integration tests:**
- Link complete component with wasmtime
- Call execute with various Python code
- Verify stdout capture works
- Test native extension imports (numpy, etc.)

### Step 8: Performance Optimization

**Areas to optimize:**
1. Python initialization - consider pre-initialization
2. String conversion - minimize copies between Rust/Python
3. Output capture - use more efficient mechanism than StringIO
4. State serialization - consider alternatives to pickle for large state

## File Structure After Implementation

```
crates/eryx-wasm-runtime/
├── Cargo.toml
├── build.sh
├── clock_stubs.c
├── src/
│   ├── lib.rs           # Main interpreter implementation
│   ├── python.rs        # CPython FFI and helpers
│   ├── state.rs         # State management (snapshot/restore/clear)
│   └── invoke.rs        # Host function callbacks
├── tests/
│   ├── link_test.rs
│   ├── runtime_test.rs
│   └── python_test.rs   # New: Python execution tests
└── target/
    └── liberyx_runtime.so
```

## Dependencies

```toml
[dependencies]
wit-dylib-ffi = { ... }

# For CPython FFI (choose one approach)
# Option A: Use pyo3-ffi for pre-generated bindings
pyo3-ffi = { version = "0.20", features = ["abi3-py314"] }

# Option B: Generate custom minimal bindings
# (Use bindgen in build.rs)
```

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| CPython initialization fails in WASM | Test early with minimal init, check WASI compatibility |
| Symbol resolution issues | Carefully track which symbols come from which library |
| Memory management bugs | Use RAII patterns, careful with Python refcounting |
| Performance regression | Benchmark against componentize-py baseline |
| Native extension compatibility | Test with real extensions (numpy, pydantic) early |

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
| Step 5: Import callbacks | 1 day |
| Step 6-7: Build & testing | 1-2 days |
| Step 8: Optimization | 1-2 days |
| **Total** | **6-11 days** |

## References

- [CPython C API](https://docs.python.org/3/c-api/index.html)
- [pyo3-ffi](https://docs.rs/pyo3-ffi/latest/pyo3_ffi/) - Low-level Python FFI bindings
- [componentize-py runtime](https://github.com/bytecodealliance/componentize-py/tree/main/runtime) - Reference implementation
- [eryx-wasm-runtime](../crates/eryx-wasm-runtime/) - Current stub implementation
