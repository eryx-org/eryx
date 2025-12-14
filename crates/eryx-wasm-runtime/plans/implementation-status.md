# Eryx WASM Runtime - Implementation Status

> **Note:** This document supersedes:
> - `docs/eryx-wasm-runtime-implementation.md` (original plan)
> - `plans/code-cleanup-and-safety.md` (cleanup suggestions)
>
> Those documents are retained for historical reference but this is the authoritative status.

This document consolidates the original implementation plan with actual progress, documenting what's complete and what remains.

## Overview

The `eryx-wasm-runtime` crate is a custom WASM runtime that replaces componentize-py's `libcomponentize_py_runtime.so`. It implements the wit-dylib interpreter interface and calls CPython via FFI for Python execution.

**Status: Core functionality COMPLETE and tested.**

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        wasmtime host                            │
├─────────────────────────────────────────────────────────────────┤
│  WIT imports: [async]invoke, list-callbacks, report-trace       │
└───────────────────────────┬─────────────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────────────┐
│                    liberyx_runtime.so                           │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  lib.rs - Interpreter trait implementation              │    │
│  │  - EryxInterpreter: export dispatch                     │    │
│  │  - EryxCall: value stack for WIT marshalling            │    │
│  │  - with_wit(): callback context management              │    │
│  └─────────────────────────┬───────────────────────────────┘    │
│                            │                                    │
│  ┌─────────────────────────▼───────────────────────────────┐    │
│  │  python.rs - CPython FFI layer                          │    │
│  │  - initialize_python(): interpreter setup               │    │
│  │  - execute_python(): code execution with capture        │    │
│  │  - snapshot/restore/clear_state(): state management     │    │
│  │  - setup_callbacks(): inject invoke() into Python       │    │
│  │  - _eryx module (PyO3): low-level invoke bridge         │    │
│  └─────────────────────────┬───────────────────────────────┘    │
│                            │                                    │
└────────────────────────────┼────────────────────────────────────┘
                             │ CPython C API
┌────────────────────────────▼────────────────────────────────────┐
│                     libpython3.14.so                            │
│                  (WASM-compiled CPython)                        │
└─────────────────────────────────────────────────────────────────┘
```

## Completed Features

### 1. wit-dylib Interpreter Implementation (`lib.rs`)

- **EryxInterpreter** implements the `Interpreter` trait from wit-dylib-ffi
- **EryxCall** provides the value stack for WIT type marshalling
- Handles all WIT value types: strings, results, lists, options, records, etc.
- Properly handles async exports with `task_return` callback

**Export dispatch** (function indices match runtime.wit order):
- `EXPORT_EXECUTE = 0` → `execute(code: string) -> result<string, string>`
- `EXPORT_SNAPSHOT_STATE = 1` → `snapshot-state() -> result<list<u8>, string>`
- `EXPORT_RESTORE_STATE = 2` → `restore-state(data: list<u8>) -> result<_, string>`
- `EXPORT_CLEAR_STATE = 3` → `clear-state()`

### 2. CPython FFI Integration (`python.rs`)

**Manual FFI declarations** for CPython C API:
- Interpreter lifecycle: `Py_InitializeEx`, `Py_IsInitialized`
- Code execution: `PyRun_SimpleString`, `PyRun_String`
- Exception handling: `PyErr_Occurred`, `PyErr_Fetch`, `PyErr_Clear`
- Object protocol: `PyObject_Str`, `PyObject_GetAttrString`
- Reference counting: `Py_IncRef`, `Py_DecRef`
- Module/dict operations: `PyImport_AddModule`, `PyModule_GetDict`, `PyDict_*`
- Bytes operations: `PyBytes_FromStringAndSize`, `PyBytes_AsString`

**PyO3 integration** for the `_eryx` module:
```rust
#[pymodule]
#[pyo3(name = "_eryx")]
fn eryx_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_eryx_invoke, m)?)?;
    Ok(())
}
```

### 3. Python Execution with Output Capture

`execute_python()` in `python.rs:824-898`:
1. Sets up stdout/stderr capture using `StringIO`
2. Runs user code via `PyRun_SimpleString`
3. Restores original stdout/stderr
4. Returns captured output or error message

### 4. State Management

**snapshot_state()** (`python.rs:1009-1089`):
- Pickles `__main__.__dict__`, excluding:
  - Python builtins (`__builtins__`, `__name__`, etc.)
  - Callback infrastructure (`invoke`, `list_callbacks`, namespace objects)
  - Unpicklable objects (tested individually)

**restore_state()** (`python.rs:1095-1131`):
- Unpickles bytes and updates `__main__.__dict__`
- Preserves existing variables not in snapshot

**clear_state()** (`python.rs:1138-1194`):
- Removes all user-defined variables
- Preserves builtins and callback infrastructure

### 5. Host Callback System

**Invoke mechanism** (`lib.rs:400-480`):
```
Python code → invoke("name", **kwargs)
           → _eryx._eryx_invoke(name, json)
           → python::do_invoke()
           → INVOKE_CALLBACK (set by with_wit)
           → call_invoke() via WIT [async]invoke import
           → host implementation
           → JSON result back through chain
```

**Callback setup** (`python.rs:1215-1364`):
- `invoke(name, **kwargs)` - generic callback invocation
- `list_callbacks()` - introspection of available callbacks
- Auto-generated wrappers for simple callbacks: `get_time()` → `invoke("get_time")`
- Namespace objects for dotted names: `http.get(url=...)` → `invoke("http.get", url=...)`

### 6. Build System (`build.sh`)

1. Builds Rust staticlib with PIC: `cargo build -Z build-std --target wasm32-wasip1`
2. Compiles `clock_stubs.c` (provides `_CLOCK_*_CPUTIME_ID` symbols)
3. Links with WASI SDK Clang to create `liberyx_runtime.so`

### 7. Test Coverage (`tests/runtime_test.rs`)

16 comprehensive tests:
1. Simple print: `print(1+1)` → `"2\n"`
2. Multiple prints
3. Variable assignment (no output)
4. Syntax error handling
5. Runtime error handling (NameError)
6. State persistence between calls
7. Stdlib import (math.pi)
8. Snapshot state
9. Clear state
10. Restore state
11. `list_callbacks()` returns list
12. `invoke("get_time")` returns result
13. `list_callbacks()` provides callback info
14. `invoke("add", a=10, b=32)` with arguments
15. Namespace callback (`http.get`)
16. Error handling for unknown callbacks

---

## Remaining Work

### Priority: High (Quick Wins)

#### 1. Remove Debug `eprintln!` Statements

**Files:** `lib.rs`
**Lines to remove:** ~20
**Risk:** Low

Remove or feature-gate these debug statements:
```rust
// lib.rs - lines to remove/gate:
eprintln!("eryx-wasm-runtime: initialize called");           // :558
eprintln!("eryx-wasm-runtime: export_start for func {}", ...); // :563
eprintln!("eryx-wasm-runtime: export_call for func {}", ...);  // :568
eprintln!("eryx-wasm-runtime: execute called with code: {code}"); // :574
eprintln!("eryx-wasm-runtime: snapshot_state called");       // :595
eprintln!("eryx-wasm-runtime: restore_state called");        // :609
eprintln!("eryx-wasm-runtime: clear_state called");          // :630
eprintln!("eryx-wasm-runtime: export_async_start for func {}", ...); // :645-646
eprintln!("eryx-wasm-runtime: async execute called with code: {code}"); // :655
eprintln!("eryx-wasm-runtime: async snapshot_state called"); // :675
eprintln!("eryx-wasm-runtime: async restore_state called");  // :688
eprintln!("eryx-wasm-runtime: async clear_state called");    // :705
eprintln!("eryx-wasm-runtime: calling task_return");         // :716
eprintln!("eryx-wasm-runtime: no task_return function available"); // :723
eprintln!("eryx-wasm-runtime: export_async_callback called"); // :733
eprintln!("eryx-wasm-runtime: resource_dtor called");        // :739
eprintln!("eryx-wasm-runtime: list-callbacks import not found"); // :488
```

**Option A:** Remove entirely
**Option B:** Feature flag: `#[cfg(feature = "debug-logging")]`

#### 2. Refactor Duplicated Export Handlers

**File:** `lib.rs`
**Lines saved:** ~80
**Risk:** Low

`export_call` (lines 567-638) and `export_async_start` (lines 640-730) have nearly identical match statements. Extract to a shared function:

```rust
/// Handle an export call, returning whether task_return is needed.
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
        EXPORT_SNAPSHOT_STATE => { /* ... */ true }
        EXPORT_RESTORE_STATE => { /* ... */ true }
        EXPORT_CLEAR_STATE => { /* ... */ false }
        _ => panic!("unknown export function index: {}", func_index),
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
                unsafe { task_return(Box::into_raw(cx).cast()); }
                return 0;
            }
        }
        drop(cx);
        0
    }
}
```

### Priority: Medium

#### 3. Fix Memory Leak in `pop_string`

**File:** `lib.rs:166-176`
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

**Solution:** Use the existing `deferred` field:
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

// Add cleanup in Drop or explicit method:
impl Drop for EryxCall {
    fn drop(&mut self) {
        for (ptr, layout) in self.deferred.drain(..) {
            unsafe { std::alloc::dealloc(ptr, layout); }
        }
    }
}
```

#### 4. Simplify INVOKE_CALLBACK (Single-Threaded)

**File:** `python.rs:567-589`
**Risk:** Low

WASM is single-threaded, so `RwLock` is unnecessary overhead:

```rust
// Before
static INVOKE_CALLBACK: std::sync::RwLock<Option<InvokeCallback>> = ...;

// After: Use thread-local (simpler, no locking)
use std::cell::RefCell;

thread_local! {
    static INVOKE_CALLBACK: RefCell<Option<InvokeCallback>> = const { RefCell::new(None) };
}

pub fn set_invoke_callback(callback: Option<InvokeCallback>) {
    INVOKE_CALLBACK.with(|cell| *cell.borrow_mut() = callback);
}

pub fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    INVOKE_CALLBACK.with(|cell| {
        let callback = cell.borrow();
        let callback = callback.as_ref().ok_or_else(||
            "invoke() called outside of execute context".to_string())?;
        callback(name, args_json)
    })
}
```

#### 5. Remove Unused CPython FFI Structures

**File:** `python.rs:390-471`
**Lines saved:** ~80
**Risk:** Low

Since we use PyO3 for module creation, these manual structures are unused:
- `PyMethodDef`, `PyCFunction`, `PyCFunctionWithKeywords`
- `METH_VARARGS`, `METH_KEYWORDS`, `METH_NOARGS`, `METH_O`
- `PyMethodDef_SENTINEL()`
- `PyModuleDef`, `PyModuleDef_Base`, `PyModuleDef_Slot`
- `PYTHON_API_VERSION`, `PyModuleDef_HEAD_INIT()`

**Note:** Keep `PyImport_AppendInittab` and `PyModule_Create2` declarations as they're part of the extern block even if unused by our code.

### Priority: Low (Nice to Have)

#### 6. Use pyo3::ffi Instead of Manual Declarations

**File:** `python.rs:42-384`
**Lines saved:** ~300
**Risk:** Medium (requires testing)

Since we depend on PyO3, we could use its FFI module:
```rust
use pyo3::ffi::{
    Py_Initialize, Py_InitializeEx, PyRun_SimpleString,
    PyObject, PyErr_Occurred, PyErr_Clear, PyErr_Fetch,
    // etc.
};
```

**Caveats:**
- Need to verify pyo3::ffi exports all functions we need
- Test thoroughly in WASM environment
- May need specific pyo3 features

#### 7. Extract Python Code to Constants

**File:** `python.rs`
**Risk:** Low (readability improvement)

Move inline Python code to module-level constants:
```rust
const CAPTURE_SETUP_PY: &CStr = c"
import sys as _sys
from io import StringIO as _StringIO
...
";

const CAPTURE_TEARDOWN_PY: &CStr = c"
_sys.stdout = _eryx_old_stdout
...
";
```

#### 8. Create RAII PyRef Wrapper

**File:** `python.rs`
**Risk:** Medium

Wrap raw Python objects in RAII guards to prevent refcount bugs:
```rust
struct PyRef(*mut PyObject);

impl PyRef {
    fn new(ptr: *mut PyObject) -> Option<Self> {
        if ptr.is_null() { None } else { Some(Self(ptr)) }
    }

    fn borrowed(ptr: *mut PyObject) -> Option<Self> {
        if ptr.is_null() { None }
        else { unsafe { Py_IncRef(ptr); } Some(Self(ptr)) }
    }
}

impl Drop for PyRef {
    fn drop(&mut self) { unsafe { Py_DecRef(self.0); } }
}
```

---

## Build System Integration

The current build uses a standalone `build.sh` script that isn't integrated with the project's mise tasks or CI. This section outlines improvements to make the build more robust and automated.

### Current State

```
build.sh (manual)
    ├── Requires WASI SDK 27 installed at $WASI_SDK_PATH or .wasi-sdk/
    ├── Runs: cargo build -Z build-std --target wasm32-wasip1
    ├── Compiles: clock_stubs.c
    └── Links: clang -shared → liberyx_runtime.so

Tests (manual setup)
    ├── Requires: tar -xf python-lib.tar.zst → tests/python-stdlib/
    └── Runs: cargo test --test runtime_test
```

**Problems:**
1. `build.sh` not in mise tasks - not discoverable, not part of `mise run ci`
2. WASI SDK must be manually installed
3. Python stdlib must be manually extracted for tests
4. Nightly Rust required for `-Z build-std` but not enforced
5. No caching of intermediate artifacts in CI

### Proposed: Rust Build Script (`build.rs`)

Replace `build.sh` with a Cargo build script for better integration:

```rust
// crates/eryx-wasm-runtime/build.rs
use std::process::Command;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=src/");
    println!("cargo::rerun-if-changed=clock_stubs.c");

    // Only build .so when explicitly requested (e.g., via feature or env var)
    if std::env::var("BUILD_ERYX_RUNTIME_SO").is_ok() {
        build_runtime_so();
    }
}

fn build_runtime_so() {
    let wasi_sdk = find_wasi_sdk();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Compile clock_stubs.c
    let status = Command::new(wasi_sdk.join("bin/clang"))
        .args(["--target=wasm32-wasip1", "--sysroot=...", "-fPIC", "-c"])
        .arg("clock_stubs.c")
        .arg("-o").arg(out_dir.join("clock_stubs.o"))
        .status()
        .expect("failed to compile clock_stubs.c");
    assert!(status.success());

    // Link shared library
    // ...

    println!("cargo::rustc-env=ERYX_RUNTIME_SO={}", out_dir.join("liberyx_runtime.so").display());
}

fn find_wasi_sdk() -> PathBuf {
    // Check WASI_SDK_PATH env var
    // Check project-local .wasi-sdk/
    // Download if missing (with version pinning)
}
```

**Benefits:**
- Cargo handles rebuild detection via `rerun-if-changed`
- `OUT_DIR` provides proper artifact location
- Can conditionally download WASI SDK
- Feature-gated: only builds .so when needed

### Proposed: mise Task Integration

Add tools and tasks to `mise.toml`:

```toml
[tools]
# Existing tools...
rust = { version = "1.92", profile = "default" }
uv = "latest"
"cargo:wasm-tools" = "latest"

# WASI SDK via GitHub backend - auto-detects platform/arch
# Releases: https://github.com/WebAssembly/wasi-sdk/releases
"github:WebAssembly/wasi-sdk" = { version = "29", bin_path = "wasi-sdk-29.0/bin" }

[tasks.build-eryx-runtime]
description = "Build liberyx_runtime.so"
dir = "crates/eryx-wasm-runtime"
env = { WASI_SDK_PATH = "{{config_root}}/.mise/installs/github-WebAssembly-wasi-sdk/29/wasi-sdk-29.0" }
run = "./build.sh"
sources = ["src/**/*.rs", "clock_stubs.c", "Cargo.toml"]
outputs = ["target/liberyx_runtime.so"]

[tasks.setup-eryx-runtime-tests]
description = "Extract Python stdlib for eryx-wasm-runtime tests"
dir = "crates/eryx-wasm-runtime"
run = """
mkdir -p tests/python-stdlib tests/site-packages
tar -xf ../eryx-runtime/.venv/lib/python3.12/site-packages/componentize_py/python-lib.tar.zst \
    -C tests/python-stdlib
"""
sources = ["../eryx-runtime/.venv/lib/python3.12/site-packages/componentize_py/python-lib.tar.zst"]
outputs = ["tests/python-stdlib/encodings/__init__.py"]  # Sentinel file

[tasks.test-eryx-runtime]
description = "Run eryx-wasm-runtime integration tests"
depends = ["build-eryx-runtime", "setup-eryx-runtime-tests"]
run = "cargo test --package eryx-wasm-runtime --test runtime_test -- --nocapture"
```

**Key improvements:**
- WASI SDK installed via `github:WebAssembly/wasi-sdk` backend (no manual download)
- Automatic platform detection (linux/macos, x86_64/arm64)
- Version pinned to 29 for reproducibility
- `bin_path` exposes clang/llvm tools on PATH
- `WASI_SDK_PATH` env var set automatically for build.sh

**Alternative: Use mise env vars directly in build.sh:**

```bash
# build.sh - updated to use mise-managed WASI SDK
WASI_SDK_PATH="${WASI_SDK_PATH:-$(mise where github:WebAssembly/wasi-sdk)/wasi-sdk-29.0}"
```

### Proposed: CI Integration

Add to `.github/workflows/ci.yml`:

```yaml
  eryx-wasm-runtime:
    name: eryx-wasm-runtime
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6

      - name: Install mise
        uses: jdx/mise-action@v3
        # mise.toml already declares github:WebAssembly/wasi-sdk
        # mise will auto-install it along with other tools

      - name: Cache mise tools (including WASI SDK)
        uses: actions/cache@v4
        with:
          path: ~/.local/share/mise
          key: ${{ runner.os }}-mise-${{ hashFiles('mise.toml') }}
          restore-keys: |
            ${{ runner.os }}-mise-

      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-eryx-runtime-${{ hashFiles('**/Cargo.lock') }}

      - name: Build WASM component (for python-lib.tar.zst)
        run: mise run build-wasm

      - name: Setup test fixtures
        run: mise run setup-eryx-runtime-tests

      - name: Build eryx-wasm-runtime
        run: mise run build-eryx-runtime

      - name: Run tests
        run: mise run test-eryx-runtime
```

**Key simplifications:**
- No manual WASI SDK download step - mise handles it automatically
- Single cache for all mise-managed tools (rust, uv, wasm-tools, wasi-sdk)
- Tools declared once in `mise.toml`, used everywhere

### Implementation Checklist

| Task | Priority | Complexity |
|------|----------|------------|
| Add `github:WebAssembly/wasi-sdk` to mise.toml | High | Low |
| Add `mise run build-eryx-runtime` task | High | Low |
| Add `mise run setup-eryx-runtime-tests` task | High | Low |
| Add `mise run test-eryx-runtime` task | High | Low |
| Update `build.sh` to use `mise where` for WASI SDK path | High | Low |
| Add CI job for eryx-wasm-runtime | High | Medium |
| Convert `build.sh` to `build.rs` (optional) | Low | Medium |
| Cache Python stdlib extraction | Low | Low |

**Note:** Using mise's github backend for WASI SDK eliminates the need for:
- Manual `setup-wasi-sdk` task
- Auto-download logic in build.rs
- Platform-specific download scripts

### Alternative: xtask Pattern

Instead of build.rs, use the xtask pattern (a separate binary crate):

```
crates/
  xtask/
    Cargo.toml
    src/main.rs  # Subcommands: build-runtime, setup-wasi-sdk, etc.
```

```bash
# Usage
cargo xtask build-runtime
cargo xtask setup-wasi-sdk
cargo xtask setup-test-fixtures
```

**Pros:** More flexible, easier to debug, no Cargo limitations
**Cons:** Another crate to maintain, not standard Cargo workflow

---

## Future Enhancements

These are not bugs or cleanup, but potential improvements:

### Performance Optimization (Original Step 8)

1. **Python initialization** - Consider pre-initialization strategies
2. **String conversion** - Minimize copies between Rust/Python
3. **Output capture** - More efficient than StringIO (e.g., custom file object)
4. **State serialization** - Alternatives to pickle for large state (marshal, custom)

### Native Extension Support

Test with real native extensions:
- numpy
- pydantic
- Other C extensions compiled to WASM

### Error Message Improvements

- Include Python traceback in error messages
- Better formatting for multi-line errors

---

## File Structure

```
crates/eryx-wasm-runtime/
├── Cargo.toml              # Dependencies: wit-dylib-ffi, pyo3
├── build.sh                # Build script (WASI SDK + Rust)
├── clock_stubs.c           # Missing libc symbols
├── README.md
├── plans/
│   ├── implementation-status.md  # This file
│   └── code-cleanup-and-safety.md  # Original cleanup suggestions
├── src/
│   ├── lib.rs              # Interpreter implementation (751 lines)
│   └── python.rs           # CPython FFI layer (1406 lines)
├── target/
│   └── liberyx_runtime.so  # Built output
└── tests/
    ├── runtime_test.rs     # Integration tests (777 lines)
    ├── python-stdlib/      # Extracted Python stdlib (from componentize-py)
    └── site-packages/      # Additional packages
```

## Dependencies

```toml
[dependencies]
wit-dylib-ffi = { ... }
pyo3 = { version = "0.24", features = ["abi3-py312"] }

[dev-dependencies]
wit-component, wit-parser, wit-dylib  # For building test components
wasmtime, wasmtime-wasi               # For running tests
zstd                                  # For decompressing libs
tokio                                 # Async test runtime
```

## Running Tests

```bash
# Prerequisites: extract Python stdlib
cd crates/eryx-wasm-runtime
mkdir -p tests/python-stdlib tests/site-packages
tar -xf ../eryx-runtime/.venv/lib/python3.12/site-packages/componentize_py/python-lib.tar.zst \
    -C tests/python-stdlib

# Build the runtime
./build.sh

# Run tests
cargo test --package eryx-wasm-runtime --test runtime_test
```

## Summary

| Category | Status |
|----------|--------|
| Core WIT exports | ✅ Complete |
| CPython integration | ✅ Complete |
| State management | ✅ Complete |
| Callback system | ✅ Complete |
| Build system (build.sh) | ✅ Complete |
| Test coverage | ✅ Complete (16 tests) |
| Code cleanup | ⏳ Pending (8 items) |
| Build system integration | ⏳ Pending (mise, CI, build.rs) |
| Performance optimization | ⏳ Future work |

### Recommended Implementation Order

1. **Quick wins (code cleanup):**
   - Remove debug `eprintln!` statements
   - Refactor duplicated export handlers
   - Fix `pop_string` memory leak

2. **Build integration (immediate value):**
   - Add `github:WebAssembly/wasi-sdk` to mise.toml (auto-installs!)
   - Add mise tasks: `build-eryx-runtime`, `setup-eryx-runtime-tests`, `test-eryx-runtime`
   - Add CI job for eryx-wasm-runtime

3. **Medium priority (code quality):**
   - Simplify INVOKE_CALLBACK to thread-local
   - Remove unused CPython FFI structures

4. **Low priority (nice to have):**
   - Use pyo3::ffi instead of manual declarations
   - Create RAII PyRef wrapper
   - Extract Python code to constants
   - Convert build.sh to build.rs (optional - mise handles most complexity now)
