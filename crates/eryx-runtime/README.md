# eryx-runtime

Python WASM runtime component for the eryx sandbox.

## Overview

This crate contains the WIT (WebAssembly Interface Types) definition and Python runtime source that gets compiled to a WebAssembly component using `componentize-py`.

## Files

- `runtime.wit` - WIT interface definition (source of truth)
- `runtime.py` - Python runtime implementation
- `runtime.wasm` - Compiled WASM component (generated, not committed)

## Prerequisites

You need `componentize-py` with async support (version >= 0.19.0):

```bash
# Create a virtual environment
python3 -m venv .venv
source .venv/bin/activate

# Install componentize-py
pip install 'componentize-py>=0.19.0'
```

Verify the version:

```bash
componentize-py --version
# Should be >= 0.19.0
```

## Building the WASM Component

To build (or rebuild) the WASM component after modifying `runtime.wit` or `runtime.py`:

```bash
cd crates/eryx-runtime

# First generate the bindings (required for async support)
componentize-py -d runtime.wit -w sandbox bindings guest_bindings

# Then build the component
componentize-py -d runtime.wit -w sandbox componentize runtime -o runtime.wasm
```

## Verifying Async Bindings

After building, check that async bindings were generated:

```bash
grep "async def invoke" guest_bindings/wit_world/__init__.py
grep "async def execute" guest_bindings/wit_world/__init__.py
```

Both should show `async def`.

## WIT Interface

The runtime exposes the `eryx:sandbox` world with:

### Imports (Host → Guest)

- `invoke(name, arguments-json) -> result<string, string>` - Async callback invocation
- `list-callbacks() -> list<callback-info>` - Introspection of available callbacks
- `report-trace(lineno, event-json, context-json)` - Execution tracing

### Exports (Guest → Host)

- `execute(code) -> result<string, string>` - Execute Python code with top-level await support

## Usage in Python

```python
# Simple callback invocation
result = await invoke("get_time", "{}")
print(result)

# Parallel execution
results = await asyncio.gather(
    invoke("query", '{"q": "a"}'),
    invoke("query", '{"q": "b"}'),
)

# Introspection
callbacks = list_callbacks()
for cb in callbacks:
    print(f"{cb['name']}: {cb['description']}")
```

## How Async Works

1. **WIT declares `async func`**: Both `invoke` (import) and `execute` (export) are async
2. **componentize-py generates async Python bindings**: Functions are async in the generated bindings
3. **Top-level await support**: Code is compiled with `PyCF_ALLOW_TOP_LEVEL_AWAIT` flag
4. **Coroutine handling**: When compiled code has top-level await, we create a function from the code object and await it
5. **wasmtime 39+**: The Rust host uses async component model support

## References

- [componentize-py](https://github.com/bytecodealliance/componentize-py)
- [Component Model Async Proposal](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Async.md)
- [wasmtime component bindgen](https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html)