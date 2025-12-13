# Native Python Extensions Research Summary

This document summarizes the research and findings from exploring how to support native Python extensions (like numpy, pydantic-core) in the eryx sandbox without requiring recompilation for every combination of extensions.

## Problem Statement

The eryx sandbox runs Python code in a WebAssembly component. When users want to use native Python extensions (C extensions compiled to WASM), the current approach using `componentize-py` requires:

1. Re-running `componentize-py` for each unique combination of extensions
2. Embedding a "symbols" dispatch table that maps WIT exports to Python methods
3. ~5-10 seconds of compilation time per combination

This is problematic because:
- Different users need different extension combinations
- The compilation cost is paid at sandbox creation time
- Caching helps but doesn't eliminate the fundamental overhead

## Approaches Explored

### Approach 1: componentize-py CLI (Current)

**How it works:**
- Run `componentize-py componentize` with native extensions in PYTHONPATH
- componentize-py generates the complete component including dispatch logic

**Pros:**
- Works today
- Handles all the complexity internally

**Cons:**
- 5-10 second compilation per unique combination
- Requires componentize-py binary and Python environment
- Opaque - hard to optimize or customize

### Approach 2: wit-component Linker (Late Linking)

**How it works:**
- Pre-build base libraries (libc, libpython, runtime) once
- Use `wit_component::Linker` to combine base + extensions at runtime
- Cache linked components by extension hash

**Pros:**
- Much faster linking (~0.5s vs ~10s)
- Can cache effectively
- More control over the process

**Cons:**
- Still requires `libcomponentize_py_runtime.so` which contains Python dispatch logic
- The runtime expects specific "symbols" to dispatch exports to Python code
- Can't easily customize the runtime behavior

### Approach 3: Custom wit-dylib Runtime (Chosen Approach)

**How it works:**
- Build a custom runtime (`liberyx_runtime.so`) that implements the wit-dylib interpreter interface
- Hardcode the eryx sandbox exports (execute, snapshot-state, restore-state, clear-state)
- Link with libpython directly via FFI for actual execution

**Pros:**
- No dependency on componentize-py's symbols dispatch
- Smaller runtime (~200KB vs ~360KB compressed)
- Full control over Python initialization and execution
- Can optimize specifically for eryx's use case
- Native extensions become truly "late-linkable"

**Cons:**
- More implementation work
- Need to handle CPython FFI ourselves
- Must match wit-dylib's expected interface exactly

## Technical Deep Dive: wit-dylib Architecture

### How componentize-py Works

```
┌─────────────────────────────────────────────────────────────────┐
│                         WIT World                                │
│   sandbox world { export execute; export snapshot-state; ... }  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ wit-dylib
┌─────────────────────────────────────────────────────────────────┐
│              libcomponentize_py_bindings.so                      │
│   Generated WASM that calls into interpreter for exports         │
│   References: libcomponentize_py_runtime.so                      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ calls interpreter functions
┌─────────────────────────────────────────────────────────────────┐
│              libcomponentize_py_runtime.so                       │
│   Implements Interpreter trait from wit-dylib-ffi                │
│   Uses "symbols" to dispatch to Python methods                   │
│   Manages Python interpreter lifecycle                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ calls Python
┌─────────────────────────────────────────────────────────────────┐
│                     libpython3.14.so                             │
│   CPython interpreter                                            │
└─────────────────────────────────────────────────────────────────┘
```

### The wit-dylib-ffi Interface

wit-dylib generates WASM bindings that call into an "interpreter" via these C functions:

```c
// Initialization
void wit_dylib_initialize(void* wit);

// Export lifecycle
void* wit_dylib_export_start(size_t which);        // Create call context
void wit_dylib_export_call(void* cx, size_t which); // Sync export
uint32_t wit_dylib_export_async_call(void* cx, size_t which); // Async export
void wit_dylib_export_finish(void* cx, size_t which); // Cleanup

// Value passing (stack-based)
void wit_dylib_push_string(void* cx, char* ptr, size_t len);
size_t wit_dylib_pop_string(void* cx, char** ptr);
void wit_dylib_push_result(void* cx, size_t ty, uint32_t discr);
// ... many more for all WIT types
```

### Async Export Handling

For async exports (which eryx uses), the flow is:

1. `wit_dylib_export_start(which)` - Create call context
2. `wit_dylib_export_async_call(cx, which)` - Start async operation
3. Inside the runtime:
   - Pop arguments from call context
   - Do the work
   - Push results to call context
   - Call `func.task_return()(cx)` to signal completion
4. Return 0 (sync completion) or task handle (for true async)

**Critical finding:** The `task_return` call is required for async exports to properly return their values. Without it, wasmtime reports "async-lifted export failed to produce a result".

## Build Process for WASI Dynamic Libraries

Building a WASM dynamic library with proper `@dylink.0` metadata requires:

### 1. Rust staticlib with PIC

```bash
RUSTFLAGS="-C relocation-model=pic" \
rustup run nightly cargo build \
    -Z build-std=panic_abort,std \
    --target wasm32-wasip1 \
    --release
```

**Key points:**
- Must use nightly Rust with `-Z build-std` to rebuild std with PIC
- The `relocation-model=pic` flag is essential
- Produces a `.a` staticlib, not a dynamic library directly

### 2. Link with Clang -shared

```bash
clang --target=wasm32-wasip1 --sysroot="$WASI_SYSROOT" \
    -shared \
    -Wl,--allow-undefined \
    -o liberyx_runtime.so \
    -Wl,--whole-archive target/.../liberyx_wasm_runtime.a -Wl,--no-whole-archive \
    clock_stubs.o
```

**Key points:**
- WASI SDK 27+ required for `-shared` support
- `--allow-undefined` needed for imports
- `--whole-archive` ensures all symbols are included
- Produces proper `@dylink.0` custom section

### 3. Missing Symbols

Some symbols expected by Rust's libc crate aren't provided by wasi-libc:

```c
// clock_stubs.c
int _CLOCK_PROCESS_CPUTIME_ID = 2;
int _CLOCK_THREAD_CPUTIME_ID = 3;
```

## Component Linking

Using `wit_component::Linker`:

```rust
let mut linker = Linker::default()
    .validate(true)
    .use_built_in_libdl(true);

// Order matters for symbol resolution
linker = linker
    .library("libc.so", &libc, false)?
    .library("libc++.so", &libcxx, false)?
    .library("libc++abi.so", &libcxxabi, false)?
    .library("libpython3.14.so", &libpython, false)?
    .library("liberyx_runtime.so", &runtime, false)?  // Our runtime
    .library("libwasi-emulated-mman.so", &wasi_mman, false)?
    // ... other WASI emulation libraries
    .library("bindings.so", &bindings, false)?;  // WIT bindings

// Native extensions are dl_openable
for ext in extensions {
    linker = linker.library(&ext.name, &ext.bytes, true)?;
}

linker = linker.adapter("wasi_snapshot_preview1", &adapter)?;
let component = linker.encode()?;
```

## Size Comparison

| Component | Size |
|-----------|------|
| liberyx_runtime.so (compressed) | 206 KB |
| libcomponentize_py_runtime_async.so (compressed) | 364 KB |
| Linked component with eryx runtime | ~3.1 MB |
| Linked component with componentize-py | ~31.3 MB |

The eryx runtime component is smaller because it doesn't currently include libpython. Once CPython FFI is added, sizes will be comparable.

## Wasmtime Requirements

For async exports, wasmtime needs:

```rust
let mut config = Config::new();
config.async_support(true);
config.wasm_component_model(true);
config.wasm_component_model_async(true);  // Required for async exports
```

The component model async feature enables the async lifting/lowering that's used for streaming and concurrent imports/exports.

## References

- [wit-dylib crate](https://github.com/AmbientRun/wit-dylib) - Generates WASM dynamic libraries from WIT
- [wit-component Linker](https://docs.rs/wit-component/latest/wit_component/struct.Linker.html) - Links WASM modules into components
- [componentize-py](https://github.com/bytecodealliance/componentize-py) - Python component toolchain
- [WASI SDK](https://github.com/WebAssembly/wasi-sdk) - WebAssembly System Interface SDK
- [Component Model Async](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Async.md) - Async component model spec
