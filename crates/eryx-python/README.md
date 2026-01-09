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
- **Fast Startup**: Embedded pre-compiled runtime for minimal initialization overhead
- **Package Support**: Load Python packages (.whl, .tar.gz) including native extensions
- **Type Safe**: Full type stubs for IDE support and static analysis

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

You can load Python packages (wheels or tar.gz archives) into the sandbox:

```python
import eryx

# Load packages from wheel files
sandbox = eryx.Sandbox(
    packages=[
        "/path/to/jinja2-3.1.2-py3-none-any.whl",
        "/path/to/markupsafe-2.1.3-wasi.tar.gz",  # WASI-compiled native extension
    ]
)

result = sandbox.execute('''
from jinja2 import Template
template = Template("Hello, {{ name }}!")
print(template.render(name="World"))
''')
```

For packages with native extensions (like markupsafe), you need WASI-compiled
versions. These are automatically late-linked into the WebAssembly component.

You can also mount a pre-extracted site-packages directory:

```python
sandbox = eryx.Sandbox(site_packages="/path/to/site-packages")
```

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