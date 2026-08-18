# eryx-runtime

Python WASM runtime component for the eryx sandbox.

## CPython Version

This runtime uses **CPython 3.14** compiled for WASI (WebAssembly System Interface).

The WASI-compiled CPython (`libpython3.14.so`) is sourced from
[componentize-py](https://github.com/bytecodealliance/componentize-py), a Bytecode Alliance
project that provides the foundational tooling for running Python in WebAssembly.

The generic WASI support libraries (`libc.so`, `libc++.so`, `libc++abi.so`, and the
`libwasi-emulated-*.so` shims) are sourced directly from the
[wasi-sdk](https://github.com/WebAssembly/wasi-sdk) release's `wasm32-wasip2` sysroot
rather than from componentize-py — they aren't CPython-specific, and componentize-py's
own `wasm32-wasip2` sysroot libs (it targets wasip2 too, as of this vintage) are the same
artifacts wasi-sdk ships directly. `libpython3.14.so` still expects a `wasm32-wasip2`-style
libc underneath it (it needs full BSD socket symbols, which only the wasip2 sysroot's libc
provides — the `wasm32-wasip1` sysroot's libc does not), even though `eryx-wasm-runtime`
itself still compiles for `wasm32-wasip1` and is bridged in via the `wasi_snapshot_preview1`
adapter below.

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
