# eryx-runtime

Python WASM runtime component for the eryx sandbox.

## CPython Version

This runtime uses **CPython 3.14** compiled for WASI (WebAssembly System Interface).

The WASI-compiled CPython and supporting libraries are built via the
[componentize-py](https://github.com/bytecodealliance/componentize-py) `main` branch's own
`cargo build` pipeline (`WASI_SDK_VERSION=33`, matching its CI), which downloads and compiles
CPython 3.14 from `dicej/cpython`'s `v3.14.0-wasi-sdk-30` source tarball against wasi-sdk-33
tools. All of `libpython3.14.so` and the generic WASI sysroot libs (`libc.so`, `libc++.so`,
`libc++abi.so`, `libwasi-emulated-*.so`) come from this single build's `OUT_DIR`, so they're
guaranteed to be built consistently against the same wasi-sdk/wasi-libc revision — mixing a
freshly-built libc with an old libpython (or vice versa) silently breaks CPython's
reference-counting-based object finalization (`__del__`/`gc.collect()` never fire), so these
files must always be re-vendored together as a set. See `docs/wasi-sdk-wasip2-migration.md`
for the full investigation.

The `wasi_snapshot_preview1` adapter (`wasi_snapshot_preview1.reactor.wasm`) is a separate,
much more stable artifact from componentize-py's `adapters/` directory (bridges
`eryx-wasm-runtime`'s own `wasm32-wasip1` build into the component) and doesn't need to be
re-vendored in lockstep with the CPython/libc bundle above.

## Overview

This crate builds the runtime.wasm component by linking together:
- `eryx-wasm-runtime` - Rust crate that provides the Python execution engine
- `libpython3.14.so` - WASI-compiled CPython 3.14
- WASI support libraries (libc, libc++, etc.)

The build process uses `wit-dylib` and `wit-component` to create a fully linked WASM component.

## Files

- `runtime.wit` - WIT interface definition (source of truth)
- `runtime.wasm` - Compiled WASM component (generated)
- `runtime.cwasm` - Pre-compiled WASM for faster loading (generated)
- `libs/` - Compressed WASI libraries required for linking
- `build.rs` - Build script that orchestrates the linking process

## Building

The WASM component is built automatically when needed:

```bash
# Build with mise
mise run build-eryx-runtime

# Or directly with cargo (requires BUILD_ERYX_RUNTIME=1)
BUILD_ERYX_RUNTIME=1 cargo build --package eryx-runtime
```

The build script will:
1. Compile `eryx-wasm-runtime` for wasm32-wasip1
2. Link it with libpython and WASI libraries
3. Generate WIT bindings with `wit-dylib`
4. Produce the final `runtime.wasm` component

## Pre-compilation

For faster sandbox creation (~50x speedup), pre-compile the WASM:

```bash
mise run precompile-eryx-runtime
```

This produces `runtime.cwasm` which can be used with `Sandbox::builder().with_precompiled_file()`.

## WIT Interface

The runtime exposes the `eryx:sandbox` world with:

### Imports (Host → Guest)

- `invoke(name, arguments-json) -> result<string, string>` - Async callback invocation
- `list-callbacks() -> list<callback-info>` - Introspection of available callbacks
- `get-execution-options() -> execution-options` - Per-execution tracing and callback reuse options
- `report-trace(lineno, event-json, context-json)` - Execution tracing via `sys.settrace`

### Exports (Guest → Host)

- `execute(code) -> result<string, string>` - Execute Python code with top-level await support
- `snapshot-state() -> result<list<u8>, string>` - Capture session state
- `restore-state(data) -> result<_, string>` - Restore session state
- `clear-state()` - Clear all persistent state

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     runtime.wasm                             │
├─────────────────────────────────────────────────────────────┤
│  liberyx_bindings.so (wit-dylib generated)                  │
│    ↓ calls                                                  │
│  liberyx_runtime.so (eryx-wasm-runtime crate)               │
│    ↓ FFI                                                    │
│  libpython3.14.so (CPython for WASI)                        │
│    ↓ uses                                                   │
│  libc.so, libc++.so, WASI emulation libs                    │
└─────────────────────────────────────────────────────────────┘
```

## References

- [wit-dylib](https://github.com/aspect-build/rules_aspect/tree/main/aspect/wit-dylib)
- [wit-component](https://github.com/bytecodealliance/wasm-tools)
- [Component Model](https://github.com/WebAssembly/component-model)
