# PyEryx - Python Bindings for Eryx

Python bindings for the [Eryx](https://github.com/sd2k/eryx) sandbox - execute Python code securely inside WebAssembly.

## Installation

```bash
pip install pyeryx
```

> **Note:** The package is installed as `pyeryx` but imported as `eryx`.

Or build from source using [maturin](https://github.com/PyO3/maturin):

```bash
cd crates/eryx-python
maturin develop
```

## Quick Start

```python
import eryx

# Create a sandbox with the embedded Python runtime
sandbox = eryx.Sandbox()

# Execute Python code in complete isolation
result = sandbox.execute('''
print("Hello from the sandbox!")
x = 2 + 2
print(f"2 + 2 = {x}")
''')

print(result.stdout)
# Output:
# Hello from the sandbox!
# 2 + 2 = 4

print(f"Execution took {result.duration_ms:.2f}ms")
```

## Features

- **Complete Isolation**: Sandboxed code cannot access files, network, or system resources
- **Resource Limits**: Configure timeouts and memory limits
- **Fast Startup**: Pre-initialized Python runtime embedded for ~1-5ms sandbox creation
- **Pre-initialization**: Custom snapshots with packages for even faster specialized sandboxes
- **Package Support**: Load Python packages (.whl, .tar.gz) including native extensions
- **Type Safe**: Full type stubs for IDE support and static analysis

## Python Version

The sandbox runs **CPython 3.14** compiled to WebAssembly (WASI). This is the same
Python build used by [componentize-py](https://github.com/bytecodealliance/componentize-py)
from the Bytecode Alliance.

> **Note:** Python 3.14 is currently in development. The sandbox tracks the latest
> WASI-compatible CPython build from the componentize-py project.

## Performance

The `pyeryx` package ships with a **pre-initialized Python runtime** embedded in the binary.
This means Python's interpreter initialization (~450ms) has already been done at build time,
so creating a sandbox is very fast:

```python
import eryx
import time

# First sandbox - fast! (~1-5ms)
start = time.perf_counter()
sandbox = eryx.Sandbox()
print(f"Sandbox created in {(time.perf_counter() - start) * 1000:.1f}ms")

# Execution is also fast
start = time.perf_counter()
result = sandbox.execute('print("Hello!")')
print(f"Execution took {(time.perf_counter() - start) * 1000:.1f}ms")
```

For repeated sandbox creation with custom packages, see
[`SandboxFactory`](#sandboxfactory) below.

## API Reference

### `Sandbox`

The main class for executing Python code in isolation.

```python
sandbox = eryx.Sandbox(
    resource_limits=eryx.ResourceLimits(
        execution_timeout_ms=5000,      # 5 second timeout
        max_memory_bytes=100_000_000,   # 100MB memory limit
    )
)

result = sandbox.execute("print('Hello!')")
```

### Loading Packages

To use custom packages, use `SandboxFactory` which bundles packages into a reusable
runtime snapshot:

```python
import eryx

# Create a factory with your packages (one-time, takes 3-5 seconds)
factory = eryx.SandboxFactory(
    packages=[
        "/path/to/jinja2-3.1.2-py3-none-any.whl",
        "/path/to/markupsafe-2.1.3-wasi.tar.gz",  # WASI-compiled native extension
    ],
    imports=["jinja2"],  # Optional: pre-import for faster first execution
)

# Create sandboxes with packages already loaded (~10-20ms each)
sandbox = factory.create_sandbox()
result = sandbox.execute('''
from jinja2 import Template
template = Template("Hello, {{ name }}!")
print(template.render(name="World"))
''')
```

For packages with native extensions (like markupsafe), you need WASI-compiled
versions. These are automatically late-linked into the WebAssembly component.

### `ExecuteResult`

Returned by `sandbox.execute()` with execution results:

- `stdout: str` - Captured standard output
- `duration_ms: float` - Execution time in milliseconds
- `callback_invocations: int` - Number of callback invocations
- `peak_memory_bytes: Optional[int]` - Peak memory usage (if available)

### `ResourceLimits`

Configure execution constraints:

```python
limits = eryx.ResourceLimits(
    execution_timeout_ms=30000,        # Max script runtime (default: 30s)
    callback_timeout_ms=10000,         # Max single callback time (default: 10s)
    max_memory_bytes=134217728,        # Max memory (default: 128MB)
    max_callback_invocations=1000,     # Max callbacks (default: 1000)
)

# Or create unlimited (use with caution!)
unlimited = eryx.ResourceLimits.unlimited()
```

### `SandboxFactory`

For use cases with **custom packages**, `SandboxFactory` lets you create a
reusable factory with your packages pre-loaded and pre-imported.

> **Note:** For basic usage without packages, `eryx.Sandbox()` is already fast (~1-5ms)
> because the base runtime ships pre-initialized. Use `SandboxFactory` only when
> you need to bundle custom packages.

Use cases for `SandboxFactory`:

- Load packages (jinja2, numpy, etc.) once and create many sandboxes from the factory
- Pre-import modules to eliminate import overhead on first execution
- Save/load factory state to disk for persistence across process restarts

```python
import eryx

# One-time factory creation with packages (takes 3-5 seconds)
factory = eryx.SandboxFactory(
    packages=[
        "/path/to/jinja2-3.1.2-py3-none-any.whl",
        "/path/to/markupsafe-2.1.3-wasi.tar.gz",
    ],
    imports=["jinja2"],  # Pre-import modules
)

# Create sandboxes with packages already loaded (~10-20ms each)
sandbox = factory.create_sandbox()
result = sandbox.execute('''
from jinja2 import Template
print(Template("Hello {{ name }}").render(name="World"))
''')

# Create many sandboxes from the same factory
for i in range(100):
    sandbox = factory.create_sandbox()
    sandbox.execute(f"print('Sandbox {i}')")
```

#### Saving and Loading

Save factories to disk for instant startup across process restarts:

```python
# Save the factory (includes pre-compiled WASM state + package state)
factory.save("/path/to/jinja2-factory.bin")

# Later, in another process - loads in ~10ms (vs 3-5s to recreate)
factory = eryx.SandboxFactory.load("/path/to/jinja2-factory.bin")
sandbox = factory.create_sandbox()
```

#### Properties and Methods

- `factory.size_bytes` - Size of the pre-compiled factory in bytes
- `factory.create_sandbox(resource_limits=...)` - Create a new sandbox
- `factory.save(path)` - Save factory to a file
- `factory.to_bytes()` - Get factory as bytes
- `SandboxFactory.load(path)` - Load factory from a file

### Exceptions

- `eryx.EryxError` - Base exception for all Eryx errors
- `eryx.ExecutionError` - Python code raised an exception
- `eryx.InitializationError` - Sandbox failed to initialize
- `eryx.ResourceLimitError` - Resource limit exceeded
- `eryx.TimeoutError` - Execution timed out

## Package Loading

### Supported Formats

- `.whl` - Standard Python wheels (zip archives)
- `.tar.gz` / `.tgz` - Tarballs (used by wasi-wheels project)
- Directories - Pre-extracted package directories

### Native Extensions

Packages containing native Python extensions (`.so` files compiled for WASI)
are automatically detected and late-linked into the WebAssembly component.
This allows packages like numpy, markupsafe, and others to work in the sandbox.

Note: You need WASI-compiled versions of native extensions, not regular
Linux/macOS/Windows binaries.

## Error Handling

```python
import eryx

sandbox = eryx.Sandbox()

try:
    result = sandbox.execute("raise ValueError('oops')")
except eryx.ExecutionError as e:
    print(f"Code failed: {e}")

try:
    sandbox = eryx.Sandbox(
        resource_limits=eryx.ResourceLimits(execution_timeout_ms=100)
    )
    result = sandbox.execute("while True: pass")
except eryx.TimeoutError as e:
    print(f"Timed out: {e}")
```

## Development

### Building

```bash
# Install maturin
pip install maturin

# Build and install in development mode
maturin develop

# Build release wheel
maturin build --release
```

### Testing

```bash
pip install pytest
pytest
```

## License

MIT OR Apache-2.0