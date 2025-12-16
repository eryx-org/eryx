# Eryx Architecture: Complete Design Overview

**Version:** December 2025
**Status:** Production-ready with async callbacks and native extension support

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [High-Level Architecture](#high-level-architecture)
3. [Component Structure](#component-structure)
4. [Virtual Filesystem (VFS)](#virtual-filesystem-vfs)
5. [Dynamic Library System](#dynamic-library-system)
6. [Build Pipeline](#build-pipeline)
7. [Late-Linking for Native Extensions](#late-linking-for-native-extensions)
8. [File Sizes](#file-sizes)
9. [Comparison with componentize-py](#comparison-with-componentize-py)
10. [Performance Characteristics](#performance-characteristics)

---

## Executive Summary

Eryx is a Python sandbox built on WebAssembly Component Model. It enables:

- **Secure Python execution** with resource limits and isolated environments
- **Async callbacks** between Python and host (Rust)
- **Native extensions** (numpy, etc.) via runtime late-linking
- **State management** (snapshot/restore) for session persistence

**Key innovation:** Custom wit-dylib runtime (`eryx-wasm-runtime`) that replaces componentize-py's runtime, enabling true late-linking of native extensions without requiring componentize-py's pre-build process.

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Host (Rust)                                    │
│                                                                          │
│  Application Code                                                        │
│    └─> Sandbox::builder()                                               │
│          ├─ .with_callback("func", callback_impl)                       │
│          ├─ .with_python_stdlib("/path/to/stdlib")                      │
│          ├─ .with_site_packages("/path/to/packages")  [for numpy etc]   │
│          ├─ .with_native_extension("numpy/core/*.so", bytes) [optional] │
│          └─ .build()                                                     │
│              │                                                           │
│              ├─ If extensions → late-link with wit_component::Linker    │
│              └─ Else → load pre-built runtime.wasm                      │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  PythonExecutor (wasmtime host)                                 │    │
│  │    ├─ Engine (async component model support)                    │    │
│  │    ├─ Store with ExecutorState                                  │    │
│  │    │    ├─ WASI context (stdio, preopened dirs)                 │    │
│  │    │    ├─ Callback channels (mpsc)                             │    │
│  │    │    ├─ Trace channel                                        │    │
│  │    │    └─ ResourceLimits enforcement                           │    │
│  │    └─ Sandbox instance (WIT bindings)                           │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              │ WIT boundary (async exports/imports)     │
└──────────────────────────────┼───────────────────────────────────────────┘
                               │
                               ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                    Guest (WASM Component)                                │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  runtime.wasm (Component Model + WASI Preview 2)                │    │
│  │                                                                  │    │
│  │  Exports (to host):                                             │    │
│  │    • execute(code: string) -> result<string, string>  [async]   │    │
│  │    • snapshot-state() -> list<u8>                               │    │
│  │    • restore-state(state: list<u8>)                             │    │
│  │    • clear-state()                                              │    │
│  │                                                                  │    │
│  │  Imports (from host):                                           │    │
│  │    • invoke(name, args) -> result<string, string>  [async]      │    │
│  │    • list-callbacks() -> list<callback-info>                    │    │
│  │    • report-trace(event)                                        │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  liberyx_bindings.so (wit-dylib generated)                      │    │
│  │    • Marshals WIT types to/from stack-based calling convention  │    │
│  │    • Calls interpreter functions in liberyx_runtime.so          │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  liberyx_runtime.so (eryx-wasm-runtime crate)                   │    │
│  │    • Implements wit-dylib interpreter interface                 │    │
│  │    • Python execution via CPython FFI (pyo3::ffi)               │    │
│  │    • Async callback suspend/resume protocol                     │    │
│  │    • State management (pickle-based snapshots)                  │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  libpython3.14.so (CPython WASM build)                          │    │
│  │    • Full Python 3.14 interpreter                               │    │
│  │    • Compiled for wasm32-wasip1 target                          │    │
│  │    • ~28MB uncompressed, ~7MB compressed                        │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  libc.so, libc++.so, libc++abi.so                               │    │
│  │    • C/C++ standard library (WASI SDK)                          │    │
│  │    • Foundation for all C/C++ code                              │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  Native Extensions (optional, late-linked)                      │    │
│  │    • numpy core, linalg, random, fft (19 .so files, ~26MB)      │    │
│  │    • pydantic-core, regex, etc.                                 │    │
│  │    • Registered as dl_openable in built-in libdl                │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  libwasi-emulated-* (WASI Preview 1 polyfills)                  │    │
│  │    • mman (memory management)                                   │    │
│  │    • signal (signal handling)                                   │    │
│  │    • process-clocks (timing)                                    │    │
│  │    • getpid (process ID)                                        │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │  wasi_snapshot_preview1 adapter                                 │    │
│  │    • Translates WASI Preview 1 calls to Preview 2               │    │
│  │    • Enables legacy WASI compatibility                          │    │
│  └────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## Component Structure

### Layered Architecture

The eryx runtime is organized in layers, from bottom to top:

```
┌───────────────────────────────────────────────────────────────┐
│ Layer 5: Python Compatibility Shims                           │
│   componentize_py_runtime.py, componentize_py_async_support   │
│   • Maps componentize-py API to _eryx module                  │
│   • Provides invoke_async(), promise_get_result(), etc.       │
└───────────────────────────────────────────────────────────────┘
                            │
┌───────────────────────────────────────────────────────────────┐
│ Layer 4: Python Interpreter (libpython3.14.so)                │
│   • CPython 3.14 compiled to WASM                             │
│   • Full standard library support                             │
│   • Top-level await enabled (PyCF_ALLOW_TOP_LEVEL_AWAIT)      │
└───────────────────────────────────────────────────────────────┘
                            │
┌───────────────────────────────────────────────────────────────┐
│ Layer 3: Runtime Bridge (liberyx_runtime.so)                  │
│   • CPython FFI (pyo3::ffi + custom bindings)                 │
│   • Python execution: execute_python()                        │
│   • Callback infrastructure: call_invoke_async()              │
│   • Async suspend/resume: PENDING_IMPORTS storage             │
│   • State management: snapshot via pickle                     │
└───────────────────────────────────────────────────────────────┘
                            │
┌───────────────────────────────────────────────────────────────┐
│ Layer 2: WIT Bindings (liberyx_bindings.so)                   │
│   • Generated by wit-dylib from runtime.wit                   │
│   • Stack-based calling convention                            │
│   • Calls wit_dylib_export_*, wit_dylib_push_*, etc.          │
└───────────────────────────────────────────────────────────────┘
                            │
┌───────────────────────────────────────────────────────────────┐
│ Layer 1: Component Model Wrapper                              │
│   • Provides Component Model interface                        │
│   • WASI Preview 2 adapter                                    │
│   • Built-in libdl (fake dlopen/dlsym via lookup tables)      │
└───────────────────────────────────────────────────────────────┘
                            │
┌───────────────────────────────────────────────────────────────┐
│ Layer 0: Base Libraries                                       │
│   • libc, libc++, libc++abi (C/C++ runtime)                   │
│   • libwasi-emulated-* (WASI polyfills)                       │
│   • Native extensions (if late-linked)                        │
└───────────────────────────────────────────────────────────────┘
```

---

## Virtual Filesystem (VFS)

Python code and packages are made available via WASI filesystem mounting (preopened directories).

### VFS Layout

```
/                                    (WASI root - no access)
├─ /python-stdlib/                   (mounted from host)
│  ├─ encodings/                     (required for Python init)
│  ├─ collections/
│  ├─ asyncio/
│  ├─ json/
│  └─ ... (full Python 3.14 stdlib)
│
├─ /site-packages/                   (mounted from host, optional)
│  ├─ numpy/
│  │  ├─ __init__.py                 (pure Python)
│  │  ├─ core/
│  │  │  ├─ __init__.py
│  │  │  └─ _multiarray_umath.*.so  (native ext, via dlopen)
│  │  ├─ linalg/
│  │  └─ random/
│  ├─ pandas/
│  └─ ... (user packages)
│
├─ /tmp/                             (WASI temp dir)
└─ /                                 (other WASI dirs as needed)
```

### How Mounting Works

```rust
// In PythonExecutor::execute()
let mut wasi_ctx = WasiCtxBuilder::new()
    .inherit_stdio()    // stdout/stderr go to host
    .inherit_env();     // Pass through environment

// Mount Python stdlib (REQUIRED)
if let Some(stdlib_path) = &self.python_stdlib_path {
    wasi_ctx = wasi_ctx.preopened_dir(
        Dir::from_std_file(File::open(stdlib_path)?),
        "/python-stdlib",
    )?;
}

// Mount site-packages (for numpy, pandas, etc.)
if let Some(site_path) = &self.python_site_packages_path {
    wasi_ctx = wasi_ctx.preopened_dir(
        Dir::from_std_file(File::open(site_path)?),
        "/site-packages",
    )?;
}
```

### Python sys.path Configuration

The runtime configures Python to search these locations:

```python
# Set in liberyx_runtime.so initialization
import sys
sys.path = [
    '/python-stdlib',
    '/site-packages',
    # ... other paths
]
```

When Python executes `import numpy`, it:
1. Searches `/site-packages/numpy/__init__.py` (finds it via VFS mount)
2. Executes the module, which does `from . import core`
3. Searches `/site-packages/numpy/core/__init__.py`
4. That module does `from . import _multiarray_umath` (native extension)
5. Python calls `dlopen("/site-packages/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so")`
6. The built-in libdl looks up that path in its pre-registered table
7. Returns handle to the linked native extension

---

## Dynamic Library System

### WASM "Dynamic Linking" is Static Lookup

In native systems, `dlopen()` loads libraries from disk at runtime. In WASM:

1. **All libraries are linked at component build/link time**
2. **dlopen() is a lookup table** implemented by wit-component's built-in libdl
3. **Libraries marked as `dl_openable=true`** are registered in the table
4. **dlsym()** returns function table indices for `call_indirect`

```rust
// At component link time
let linker = Linker::default()
    .use_built_in_libdl(true)      // Enable fake dlopen
    .library("libc.so", bytes, false)              // NOT dl_openable
    .library("libpython3.14.so", bytes, false)     // NOT dl_openable
    .library("/site-packages/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so",
             bytes,
             true)  // ← dl_openable = true (goes in dlopen table)
    .encode()?;
```

### dlopen Lookup Table Structure

The built-in libdl creates a data structure in linear memory:

```
┌────────────────────────────────────────────────────┐
│  DLOpen Table (sorted by path for binary search)   │
├────────────────────────────────────────────────────┤
│  "/site-packages/numpy/core/_multiarray_umath..." │
│    → Library Handle 1                              │
│    → Symbol Table:                                 │
│       "PyInit__multiarray_umath" → func_idx 42     │
│       "PyArray_API" → data_idx 7                   │
│                                                    │
│  "/site-packages/numpy/linalg/_umath_linalg..."   │
│    → Library Handle 2                              │
│    → Symbol Table: ...                             │
└────────────────────────────────────────────────────┘
```

When Python calls `dlopen("/site-packages/numpy/core/_multiarray_umath...")`:
1. Built-in libdl does binary search on the table
2. Returns the library handle if found
3. `dlsym(handle, "PyInit__multiarray_umath")` looks up in that library's symbols
4. Returns function table index
5. Python uses `call_indirect` to invoke the function

---

## Build Pipeline

### For Base Runtime (runtime.wasm)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Build eryx-wasm-runtime                              │
│   cd crates/eryx-wasm-runtime                                │
│   cargo build --target wasm32-wasip1 -Z build-std           │
│                                                              │
│   Output: liberyx_wasm_runtime.a (staticlib with PIC)        │
│   Size: ~800KB                                               │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Compile clock_stubs.c                                │
│   wasi-sdk/bin/clang --target=wasm32-wasip1 -fPIC -c        │
│                                                              │
│   Output: clock_stubs.o                                      │
│   Purpose: Provides missing _CLOCK_* symbols                 │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Step 3: Link into shared library                            │
│   wasi-sdk/bin/clang --target=wasm32-wasip1 -shared         │
│     -Wl,--allow-undefined                                    │
│     -Wl,--whole-archive liberyx_wasm_runtime.a               │
│     -Wl,--no-whole-archive clock_stubs.o                     │
│                                                              │
│   Output: liberyx_runtime.so (with @dylink.0 metadata)       │
│   Size: 1.1MB uncompressed, ~206KB compressed                │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Step 4: Generate WIT bindings                                │
│   wit_dylib::create(resolve, world_id, opts)                │
│     interpreter = "liberyx_runtime.so"                       │
│                                                              │
│   Output: liberyx_bindings.so                                │
│   Size: ~50KB uncompressed, ~15KB compressed                 │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Step 5: Link all libraries into component                   │
│   wit_component::Linker                                      │
│     .library("libc.so", ...)                                 │
│     .library("libc++.so", ...)                               │
│     .library("libpython3.14.so", ...)                        │
│     .library("liberyx_runtime.so", ...)                      │
│     .library("liberyx_bindings.so", ...)                     │
│     .library("libwasi-emulated-*.so", ...)                   │
│     .adapter("wasi_snapshot_preview1", ...)                  │
│     .encode()                                                │
│                                                              │
│   Output: runtime.wasm                                       │
│   Size: ~31MB (includes all libraries)                       │
└─────────────────────────────────────────────────────────────┘
```

### Artifacts by Stage

| Artifact | Stage | Size (uncompressed) | Size (compressed) | Purpose |
|----------|-------|---------------------|-------------------|---------|
| `liberyx_wasm_runtime.a` | After rustc | 800KB | - | Rust staticlib |
| `liberyx_runtime.so` | After clang link | 1.1MB | 206KB | wit-dylib interpreter |
| `liberyx_bindings.so` | wit-dylib gen | 50KB | 15KB | WIT marshaling |
| `runtime.wasm` | Final component | 31MB | - | Complete component |

---

## Late-Linking for Native Extensions

When native extensions are needed, we re-link the component at sandbox creation time.

### Process Flow

```
┌─────────────────────────────────────────────────────────────┐
│ User Code:                                                   │
│   Sandbox::builder()                                         │
│     .with_native_extension("numpy/core/*.so", bytes)         │
│     .build()                                                 │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ SandboxBuilder::build()                                      │
│   if !native_extensions.is_empty() {                         │
│       component = link_with_extensions(&extensions)?;        │
│   } else {                                                   │
│       component = load_pre_built_runtime()?;                 │
│   }                                                          │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ eryx_runtime::linker::link_with_extensions()                 │
│                                                              │
│   1. Decompress base libraries from embedded bytes          │
│      - libc.so.zst → libc.so                                │
│      - libpython3.14.so.zst → libpython3.14.so              │
│      - liberyx_runtime.so.zst → liberyx_runtime.so          │
│      - ... (all base libraries)                             │
│                                                              │
│   2. Create wit_component::Linker                           │
│      - Add base libraries (dl_openable=false)               │
│      - Add native extensions (dl_openable=true)             │
│      - Add WASI adapter                                     │
│                                                              │
│   3. Encode component                                        │
│                                                              │
│   Output: Component bytes with late-linked extensions        │
│   Time: ~1-2 seconds for numpy (19 extensions)              │
└─────────────────────────────────────────────────────────────┘
```

### Base Libraries Embedded in eryx-runtime

The base libraries are compressed with zstd and embedded via `include_bytes!()`:

```rust
// In eryx-runtime/src/linker.rs
pub mod base_libraries {
    pub const LIBC: &[u8] =
        include_bytes!("../libs/libc.so.zst");
    pub const LIBPYTHON: &[u8] =
        include_bytes!("../libs/libpython3.14.so.zst");
    pub const LIBERYX_RUNTIME: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/liberyx_runtime.so.zst"));
    // ... etc
}
```

Total embedded size: ~10MB compressed

### Extension Registration

Native extensions are registered with the exact path Python will use for dlopen:

```rust
// Python does: dlopen("/site-packages/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so")
// We register it with that exact path:
builder.with_native_extension(
    "/site-packages/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so",
    extension_bytes
)
```

The path must match because:
1. Python constructs the path based on `sys.path` + module structure
2. The built-in libdl does exact string matching (or binary search)
3. Mismatch = "library not found" error

---

## File Sizes

### Base Libraries (Decompressed)

| Library | Size | Compression Ratio | Purpose |
|---------|------|-------------------|---------|
| `libpython3.14.so` | 28MB | 25.6% → 7.1MB | Python interpreter |
| `libc++.so` | 5.2MB | 21.1% → 1.1MB | C++ standard library |
| `libc.so` | 2.1MB | 33.0% → 699KB | C standard library |
| `libc++abi.so` | 1.1MB | 21.7% → 225KB | C++ ABI |
| `wasi_snapshot_preview1.reactor.wasm` | 95KB | 20.9% → 20KB | WASI adapter |
| `libwasi-emulated-*` | ~21KB total | - | WASI polyfills |
| **Total base libraries** | **~36MB** | **→ ~9MB compressed** | |

> **Note:** The `libs/` directory also contains `libcomponentize_py_runtime_*.so` files
> (vestigial from componentize-py). These are NOT used—eryx uses `liberyx_runtime.so` instead.

### Runtime Components

| Component | Size | Notes |
|-----------|------|-------|
| `liberyx_runtime.so` | 1.1MB uncompressed, ~206KB compressed | Our custom runtime |
| `liberyx_bindings.so` | ~50KB uncompressed, ~15KB compressed | WIT bindings |
| `runtime.wasm` (final) | 31MB | Complete component with all libs |

### Native Extensions (numpy example)

| Extension Category | Files | Total Size |
|-------------------|-------|------------|
| numpy core | `_multiarray_umath.so` | 12.9MB |
| numpy linalg | `_umath_linalg.so`, `lapack_lite.so` | 5.6MB |
| numpy random | 9 files | 7.4MB |
| numpy fft | `_pocketfft_internal.so` | 172KB |
| numpy tests | 4 files | 630KB |
| **Total numpy native** | **19 .so files** | **~26MB** |
| **Total numpy (with Python)** | - | **~35MB** |

### Component Size Comparison

| Configuration | Component Size | Creation Time |
|--------------|----------------|---------------|
| Base runtime (no extensions) | 31MB | ~16ms (embedded), ~500ms (from file) |
| Late-linked with numpy | ~57MB | ~1500-2000ms |
| componentize-py pre-built with numpy | ~40-50MB | ~50-100ms |

---

## Comparison with componentize-py

### Architecture Differences

```
┌──────────────────────────────────────────────────────────────┐
│ componentize-py Approach                                      │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  1. Build time:                                              │
│     componentize-py componentize runtime.py -o runtime.wasm  │
│     • Generates "symbols" dispatch table                     │
│     • Pre-initializes Python (imports modules, captures mem) │
│     • Embeds everything in one component                     │
│     • Time: 5-10 seconds per unique extension combination    │
│                                                              │
│  2. Runtime:                                                 │
│     • Fast cold starts (~50ms) due to pre-init               │
│     • Single .wasm artifact, easy to distribute              │
│     • Fixed set of extensions (can't add more)               │
│                                                              │
│  Runtime: libcomponentize_py_runtime.so                      │
│    • Generic dispatcher from WIT exports to Python methods   │
│    • Uses "symbols" table to find Python callables           │
│    • Size: ~1.6MB (364KB compressed)                         │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│ Eryx Approach                                                 │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  1. Build time (one-time setup):                             │
│     Build liberyx_runtime.so and embed base libraries        │
│     • Hardcoded exports (execute, snapshot-state, etc.)      │
│     • NO pre-initialization                                  │
│     • Time: ~30 seconds (one-time)                           │
│                                                              │
│  2. Sandbox creation time:                                   │
│     • No extensions: Use pre-built runtime.wasm (~16-500ms)  │
│     • With extensions: Late-link with wit_component::Linker  │
│       - Decompress base libraries                            │
│       - Add extension .so files                              │
│       - Link and encode                                      │
│       - Time: ~1.5-2 seconds for numpy                       │
│                                                              │
│  Runtime: liberyx_runtime.so                                 │
│    • Custom wit-dylib interpreter in Rust                    │
│    • Hardcoded eryx sandbox exports                          │
│    • Direct CPython FFI (pyo3::ffi)                          │
│    • Size: ~1.1MB (206KB compressed)                         │
└──────────────────────────────────────────────────────────────┘
```

### Feature Comparison

| Feature | componentize-py | Eryx |
|---------|----------------|------|
| **Async callbacks** | ✅ Yes | ✅ Yes |
| **Native extensions** | ✅ Pre-build only | ✅ Late-linking |
| **Runtime flexibility** | ❌ Must use componentize-py | ✅ Pure Rust, no Python toolchain |
| **Cold start time** | ✅ ~50ms (pre-init) | ⚠️ ~500ms (no pre-init) |
| **Add extensions** | ❌ Rebuild required (5-10s) | ✅ At sandbox creation (~1.5s) |
| **Component size** | ~40-50MB with numpy | ~57MB with numpy (late-linked) |
| **Build complexity** | ⚠️ Requires componentize-py CLI | ✅ Just cargo + WASI SDK |
| **State management** | ⚠️ Via pre-init memory | ✅ Via snapshot-state export |

### Pros of Eryx Approach

1. **Flexibility**: Add different extension combinations without rebuilding
2. **No Python dependency**: Pure Rust toolchain (cargo + WASI SDK)
3. **Smaller runtime**: 206KB vs 364KB for the runtime itself
4. **Full control**: We own the runtime implementation
5. **True late-linking**: Achieved the original design goal

### Cons of Eryx Approach

1. **Slower cold starts**: ~500ms vs ~50ms (no pre-initialization)
2. **Linking overhead**: ~1.5s when adding native extensions
3. **Larger embedded data**: ~10MB compressed base libraries in eryx-runtime crate
4. **API verbosity**: Must explicitly call `with_python_stdlib()` and `with_site_packages()`
5. **Less mature**: componentize-py is battle-tested in production

### When to Use Each

**Use eryx's late-linking:**
- Different users need different extension combinations
- Development/experimentation workflows
- When you want to avoid componentize-py dependency
- When you need to add extensions programmatically

**Use componentize-py:**
- Known fixed set of extensions
- Cold start performance is critical (<100ms)
- You're okay with 5-10s rebuild per combination
- You want a mature, production-proven solution

### Hybrid Approach (Future)

We could add pre-initialization to late-linked components:

```rust
let component = eryx_runtime::linker::link_with_extensions(&extensions)?;
let preinit = eryx_runtime::preinit::pre_initialize(&component).await?;
// Now preinit has ~50ms cold starts like componentize-py
```

This would combine the best of both worlds:
- Flexibility of late-linking
- Performance of pre-initialization

---

## Performance Characteristics

### Sandbox Creation Time

```
No extensions (pre-built runtime.wasm):
  • From file: ~500ms (wasmtime compile)
  • Embedded runtime: ~16ms (pre-compiled)
  • Precompiled file: ~16ms (pre-compiled)

With native extensions (late-linking):
  • Decompress base libraries: ~200ms
  • wit_component::Linker: ~1000ms
  • wasmtime compile: ~500ms
  • Total: ~1700ms for numpy
```

### Execution Time

Once created, execution performance is identical between approaches:

- Python execution: Native speed (compiled to native code by wasmtime)
- Callback overhead: ~10-50μs per callback
- Async suspend/resume: ~100-200μs

### Memory Usage

| Configuration | Peak Memory | Notes |
|--------------|-------------|-------|
| Base sandbox | ~50MB | Python interpreter + runtime |
| With numpy | ~150MB | numpy allocates large arrays |
| With pre-init | +12MB | Captured memory state |

---

## API Usage Examples

### Minimal Sandbox (No Extensions)

```rust
use eryx::Sandbox;

let sandbox = Sandbox::builder()
    .with_embedded_runtime()
    .with_python_stdlib("/path/to/stdlib")  // Required for Python init
    .build()?;

let result = sandbox.execute("print('Hello, World!')").await?;
```

### With Callbacks

```rust
#[derive(Clone)]
struct AddCallback;

#[async_trait]
impl Callback for AddCallback {
    fn name(&self) -> &str { "add" }

    async fn call(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let a = args["a"].as_i64().unwrap();
        let b = args["b"].as_i64().unwrap();
        Ok(json!(a + b))
    }
}

let sandbox = Sandbox::builder()
    .with_callback(AddCallback)
    .with_python_stdlib("/path/to/stdlib")
    .build()?;

let result = sandbox.execute(r#"
result = await add(a=5, b=3)
print(f"5 + 3 = {result}")
"#).await?;
```

### With Native Extensions (numpy)

```rust
// Load numpy .so files
let mut builder = Sandbox::builder();

for so_file in find_numpy_extensions("/tmp/numpy")? {
    let bytes = std::fs::read(&so_file)?;
    let dlopen_path = format!("/site-packages/{}", relative_path(&so_file));
    builder = builder.with_native_extension(dlopen_path, bytes);
}

// Mount Python files
let sandbox = builder
    .with_python_stdlib("/path/to/stdlib")
    .with_site_packages("/tmp")  // Contains numpy/
    .build()?;

let result = sandbox.execute(r#"
import numpy as np
a = np.array([1, 2, 3])
print(a.sum())
"#).await?;
```

---

## Async Callback Flow

### Simple Synchronous Callback

```
Python                          Host (Rust)
  │                               │
  │  result = await add(5, 3)     │
  ├──────────[async]invoke─────>  │
  │  name="add", args='{"a":5,"b":3}'
  │                               │
  │                               ├─> Callback::call()
  │                               │     return Ok(8)
  │                               │
  │  <─────────Ok('8')──────────  │
  │                               │
  │  print(result)  # 8           │
```

### Async Callback with Suspend/Resume

```
Python                          Host (Rust)                     External API
  │                               │                               │
  │  result = await fetch_url()   │                               │
  ├──────[async]invoke────────>   │                               │
  │  name="fetch_url", args='{}'  │                               │
  │                               ├──> Callback::call() [async]   │
  │                               │       tokio::spawn(...)        │
  │                               │         HTTP request ────────> │
  │                               │                               │
  │  <─────Pending(task_id)─────  │                               │
  │                               │                               │
  │  [Python suspends]            │     [waiting for HTTP]        │
  │  WAIT | waitable_set << 4     │                               │
  │  Store context in globals     │                               │
  │                               │                               │
  │                               │   <─── HTTP response ─────────│
  │                               │                               │
  │  <──export_async_callback───  │                               │
  │  event=(SUBTASK, task_id, RETURNED)                           │
  │                               │                               │
  │  [Restore CURRENT_WIT]        │                               │
  │  [Lift result from buffer]    │                               │
  │  [Resume Python execution]    │                               │
  │                               │                               │
  │  result = promise_get_result()│                               │
  │  print(result)                │                               │
```

### Key Components in Async Flow

1. **PENDING_IMPORTS** (in liberyx_runtime.so):
   - Stores `buffer`, `async_lift_impl`, and `EryxCall` for pending operations
   - Keyed by subtask ID
   - Keeps buffer alive until result is lifted

2. **PENDING_ASYNC_STATE** (in liberyx_runtime.so):
   - Stores `EryxCall`, `task_return`, and `Wit` handle for suspended exports
   - Restored when async callback completes

3. **_eryx_async_import_result** (Python global):
   - Stores lifted result as JSON
   - Read by `promise_get_result()` after callback completes

---

## State Management

### Snapshot/Restore via Pickle

```python
# Snapshot (in liberyx_runtime.so)
snapshot = {
    'globals': pickle.dumps(__main__.__dict__),
    'random_state': random.getstate(),
}
return json.dumps(snapshot)

# Restore
state = json.loads(state_json)
__main__.__dict__.update(pickle.loads(state['globals']))
random.setstate(state['random_state'])
```

### Use Cases

1. **Session persistence**: Save user's interactive session to disk
2. **Checkpointing**: Restore to known good state after errors
3. **Multi-turn agents**: Preserve context between LLM turns

### Limitations

- CPython can only `Py_Initialize()` once per process
- `reset()` doesn't work - must create new executor
- Some state (open files, threads) cannot be pickled

---

## Security Model

### Isolation Boundaries

```
┌─────────────────────────────────────────────────────────────┐
│ Host Process (Rust)                                          │
│   Full system access                                         │
└─────────────────────────────────────────────────────────────┘
                        │
                   WIT boundary
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ WASM Component (Sandboxed)                                   │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐ │
│  │ Python Interpreter                                      │ │
│  │   • No network access (unless via host callbacks)       │ │
│  │   • No filesystem access (except mounted dirs)          │ │
│  │   • No process spawning                                 │ │
│  │   • No system calls (WASI only)                         │ │
│  └────────────────────────────────────────────────────────┘ │
│                                                              │
│  Capabilities (via WASI):                                    │
│    ✓ Read mounted directories (preopened)                   │
│    ✓ Write to /tmp (if mounted)                             │
│    ✓ Invoke host callbacks (via WIT import)                 │
│    ✓ Use CPU/memory within limits                           │
│                                                              │
│    ✗ Network access                                          │
│    ✗ Arbitrary file access                                   │
│    ✗ Fork/exec                                               │
│    ✗ Native syscalls                                         │
└──────────────────────────────────────────────────────────────┘
```

### Resource Limits

```rust
pub struct ResourceLimits {
    pub execution_timeout: Option<Duration>,        // Default: 30s
    pub callback_timeout: Option<Duration>,         // Default: 10s
    pub max_memory_bytes: Option<u64>,             // Default: 256MB
    pub max_callback_invocations: Option<u32>,     // Default: 1000
}
```

Enforced via:
- `tokio::time::timeout()` for execution/callback timeouts
- wasmtime `Store::set_fuel()` for CPU limits (optional)
- wasmtime memory limits for heap size
- Counter in ExecutorState for callback limits

---

## Design Decisions & Rationale

### Why Custom Runtime (vs componentize-py)?

**Goal:** Enable late-linking of native extensions without componentize-py's pre-build requirement.

**Challenge:** componentize-py's runtime uses a "symbols" dispatch table generated at build time. This table maps WIT exports to Python methods and is baked into the component.

**Solution:** Build a custom runtime that:
1. Hardcodes the eryx sandbox exports (execute, snapshot-state, etc.)
2. Links directly to libpython via FFI
3. Doesn't need a dispatch table
4. Enables true late-linking

### Why wit-dylib Interface?

wit-dylib provides a C ABI for implementing Component Model runtimes:

```c
void wit_dylib_initialize(void* wit);
void* wit_dylib_export_start(size_t which);
void wit_dylib_export_call(void* cx, size_t which);
uint32_t wit_dylib_export_async_call(void* cx, size_t which);
// ... etc
```

This lets us write the runtime in Rust, compile to a WASM shared library, and have it work as a Component Model interpreter.

### Why WASI SDK Clang (vs rustc for linking)?

Rust's `wasm32-wasip1` target doesn't support `-C link-arg=-shared` directly. WASI SDK's clang is the canonical way to create WASM shared libraries with proper `@dylink.0` metadata.

### Why Separate Target Directory?

```rust
cmd.env("CARGO_TARGET_DIR", &nested_target_dir);
```

Cargo uses file locks on the target directory. When build.rs invokes a nested cargo build, it would deadlock trying to acquire the same lock. Using a separate target directory avoids this.

### Why Stack-Based Value Passing?

wit-dylib uses a stack-based calling convention:

```rust
// To call: execute(code: string) -> result<string, string>

// Push arguments (in reverse order for pop)
cx.push_string(code.to_string());

// Call the export
wit_dylib_export_call(cx, 0);  // export index 0 = execute

// Pop results
let discriminant = cx.stack.pop();  // result<_, _> discriminant
let value = cx.stack.pop();         // string value
```

This is simpler than C ABI calling conventions and works well with WASM's linear memory model.

---

## Future Enhancements

### 1. Pre-Initialization Support

Add optional pre-init like componentize-py:

```rust
let component = eryx_runtime::linker::link_with_extensions(&exts)?;
let preinit = eryx_runtime::preinit::pre_initialize(&component).await?;

// Now has ~50ms cold starts
let sandbox = Sandbox::builder()
    .with_precompiled_bytes(preinit)
    .build()?;
```

Benefits:
- 10x faster cold starts
- Keep late-linking flexibility

Challenges:
- Need to run wasmtime in build/link process
- Must capture memory state correctly
- Adds complexity

### 2. Component Caching

Cache late-linked components by extension hash:

```rust
let cache_key = compute_cache_key(&extensions);
if let Some(cached) = cache.get(&cache_key) {
    return Ok(cached);
}

let component = link_with_extensions(&extensions)?;
cache.put(cache_key, component.clone());
```

Benefits:
- Amortize linking cost across multiple sandboxes
- ~16ms creation time after first link

### 3. Auto-Detect Python stdlib

```rust
// Check multiple locations
fn find_python_stdlib() -> Option<PathBuf> {
    // 1. Environment variable
    if let Ok(path) = env::var("ERYX_PYTHON_STDLIB") {
        return Some(PathBuf::from(path));
    }

    // 2. Relative to runtime.wasm
    // 3. Bundled in component (adds ~50MB)
    // 4. Download from known location

    None
}

// Simplify API
let sandbox = Sandbox::builder()
    .build()?;  // Auto-finds stdlib
```

### 4. Wheel Integration

```rust
impl SandboxBuilder {
    pub async fn with_wheel(
        mut self,
        url: &str
    ) -> Result<Self, Error> {
        let bytes = download_wheel(url).await?;
        let wheel = eryx_runtime::linker::parse_wheel(&bytes)?;

        for ext in wheel.native_extensions {
            self = self.with_native_extension(&ext.name, ext.bytes);
        }

        // Extract Python files to temp dir and mount
        let temp_dir = extract_python_files(&wheel)?;
        self = self.with_site_packages(&temp_dir);

        Ok(self)
    }
}

// Usage
let sandbox = Sandbox::builder()
    .with_wheel("https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz").await?
    .build()?;
```

### 5. Smaller Runtime Variant

Create a minimal runtime without state management for ~50% size reduction:

```rust
// Minimal runtime (no snapshot/restore)
liberyx_runtime_minimal.so: ~500KB vs 1.1MB
```

---

## Appendix: Complete File Manifest

### Source Files

```
eryx/
├── crates/eryx/                     (Host crate)
│   ├── src/
│   │   ├── sandbox.rs               (8KB) - Sandbox API
│   │   ├── wasm.rs                  (12KB) - PythonExecutor
│   │   ├── session/                 - Session management
│   │   └── lib.rs                   - Public exports
│   └── examples/
│       └── numpy_native.rs          (4KB) - Late-linking demo
│
├── crates/eryx-runtime/             (Runtime builder crate)
│   ├── src/
│   │   ├── lib.rs                   (1KB) - Public exports
│   │   └── linker.rs                (14KB) - Late-linking
│   ├── build.rs                     (12KB) - Component builder
│   ├── runtime.wit                  (2KB) - WIT interface
│   └── libs/                        (~10MB compressed)
│       ├── libc.so.zst              (699KB)
│       ├── libpython3.14.so.zst    (7.1MB)
│       └── ... (all base libraries)
│
└── crates/eryx-wasm-runtime/        (Guest runtime crate)
    ├── src/
    │   ├── lib.rs                   (35KB) - wit-dylib impl
    │   └── python.rs                (28KB) - CPython FFI
    ├── python/                      (Python shims)
    │   ├── componentize_py_runtime.py
    │   ├── componentize_py_types.py
    │   └── componentize_py_async_support/
    └── tests/
        └── python-stdlib/           (~50MB) - Python stdlib
```

### Binary Artifacts

```
Build outputs:
├── liberyx_wasm_runtime.a           (800KB) - Rust staticlib
├── liberyx_runtime.so               (1.1MB) - Shared library
├── liberyx_bindings.so              (50KB) - WIT bindings
└── runtime.wasm                     (31MB) - Final component

Embedded in eryx-runtime crate:
├── Base libraries                   (~10MB compressed)
└── Generated at build time:
    ├── liberyx_runtime.so.zst       (206KB)
    └── liberyx_bindings.so.zst      (15KB)

Late-linked components:
└── runtime+numpy.wasm               (~57MB) - With 19 numpy extensions
```

---

## Summary

Eryx achieves the original design goal of supporting native Python extensions without componentize-py's pre-build requirement. The custom wit-dylib runtime enables true late-linking, where extensions can be added at sandbox creation time (~1.5s) rather than at build time (5-10s).

The tradeoff is slower cold starts (~500ms vs ~50ms) due to skipping pre-initialization, but this can be added in future if needed. The flexibility and pure-Rust approach make eryx ideal for scenarios where different users need different extension combinations or where avoiding the componentize-py dependency is important.
