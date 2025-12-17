# eryx-runtime

Python WASM runtime component for the eryx sandbox.

## Overview

This crate builds the runtime.wasm component by linking together:
- `eryx-wasm-runtime` - Rust crate that provides the Python execution engine
- `libpython3.14.so` - WASI-compiled CPython
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
