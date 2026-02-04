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

- **Complete Isolation**: Sandboxed code runs in WebAssembly with no host access by default
- **Controlled Network Access**: Optional TCP/TLS networking with host filtering and policies
- **Host Callbacks**: Expose async Python functions for controlled host interaction
- **Resource Limits**: Configure timeouts, memory limits, and callback restrictions
- **Fast Startup**: Pre-initialized Python runtime embedded for ~1-5ms sandbox creation
- **Pre-initialization**: Custom snapshots with packages for even faster specialized sandboxes
- **Package Support**: Load Python packages (.whl, .tar.gz) including native extensions
- **Persistent Sessions**: Maintain Python state across executions with `Session`
- **Virtual Filesystem**: Sandboxed file storage with `VfsStorage`
- **Type Safe**: Full type stubs for IDE support and static analysis

## Python Version

The sandbox runs **CPython 3.14** compiled to WebAssembly (WASI). This is the same
Python build used by [componentize-py](https://github.com/bytecodealliance/componentize-py)
from the Bytecode Alliance.

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

**Core Classes:**
- [`Sandbox`](#sandbox) - Main class for isolated Python execution
- [`Session`](#session) - Persistent state across executions
- [`ExecuteResult`](#executeresult) - Execution results with stdout and stats
- [`ResourceLimits`](#resourcelimits) - Configure execution constraints
- [`NetConfig`](#netconfig) - Configure network access and policies
- [`Callbacks`](#callbacks) - Expose host functions to sandboxed code
- [`SandboxFactory`](#sandboxfactory) - Pre-initialize sandboxes with packages
- [`VfsStorage`](#vfsstorage) - Virtual filesystem for sessions

### Sandbox vs Session

| Feature | `Sandbox` | `Session` |
|---------|-----------|-----------|
| State persistence | No - fresh each execute() | Yes - variables persist |
| Virtual filesystem | No | Yes (optional) |
| Use case | One-off execution | REPL, multi-step workflows |
| Isolation | Complete per-call | Complete from host |

**Use `Sandbox` when:**
- Running untrusted code that should start fresh each time
- Each execution is independent
- You want maximum isolation between executions

**Use `Session` when:**
- Building up state across multiple executions
- You need file persistence (VFS)
- Implementing a REPL or notebook-like experience
- Performance matters (no re-initialization per call)

### `Sandbox`

The main class for executing Python code in isolation.

```python
sandbox = eryx.Sandbox(
    resource_limits=eryx.ResourceLimits(
        execution_timeout_ms=5000,      # 5 second timeout
        max_memory_bytes=100_000_000,   # 100MB memory limit
    ),
    network=eryx.NetConfig(             # Optional: enable networking
        allowed_hosts=["api.example.com"]
    ),
    callbacks=[                          # Optional: host functions
        {"name": "get_data", "fn": get_data_fn, "description": "Fetch data"}
    ]
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

### `NetConfig`

Configure network access for sandboxed code. By default, **all network access is disabled**.
Enable networking by creating a `NetConfig` and passing it to the sandbox.

```python
import eryx

# Default config - allows external hosts, blocks localhost/private networks
config = eryx.NetConfig(
    max_connections=10,                    # Max concurrent connections
    connect_timeout_ms=30000,              # Connection timeout (30s)
    io_timeout_ms=60000,                   # I/O timeout (60s)
    allowed_hosts=["api.example.com"],     # Whitelist specific hosts
    blocked_hosts=[]                       # Override default blocks
)

sandbox = eryx.Sandbox(network=config)
result = sandbox.execute("""
import urllib.request
response = urllib.request.urlopen("https://api.example.com/data")
print(response.read().decode())
""")
```

#### Security Defaults

By default, `NetConfig` blocks localhost and private networks to prevent SSRF attacks:
- `localhost`, `127.*`, `[::1]`
- Private networks: `10.*`, `172.16.*`-`172.31.*`, `192.168.*`, `169.254.*`

#### Permissive Configuration

For testing or development, use `.permissive()` to allow all hosts including localhost:

```python
# WARNING: Allows sandbox to access local services
config = eryx.NetConfig.permissive()
sandbox = eryx.Sandbox(network=config)
```

#### Host Filtering

Control which hosts sandboxed code can connect to using patterns with wildcards:

```python
config = eryx.NetConfig(
    allowed_hosts=[
        "api.example.com",           # Exact host
        "*.googleapis.com",          # Wildcard subdomain
        "api.*.com",                 # Wildcard in middle
    ]
)
```

When `allowed_hosts` is non-empty, only matching hosts are allowed. Blocked hosts are checked first.

#### Builder Methods

Chain methods for convenient configuration:

```python
config = (eryx.NetConfig()
    .allow_host("api.example.com")
    .allow_host("*.openai.com")
    .allow_localhost()               # Remove localhost from blocked list
    .with_root_cert(cert_der_bytes)) # Add custom CA cert for self-signed certs
```

#### Custom Certificates

Add custom root certificates for testing with self-signed certificates:

```python
# Load certificate in DER format
with open("ca-cert.der", "rb") as f:
    cert_der = f.read()

config = eryx.NetConfig().with_root_cert(cert_der)
sandbox = eryx.Sandbox(network=config)
```

#### Supported Protocols

Network-enabled sandboxes support:
- **HTTP/HTTPS** via `urllib.request`, `http.client`
- **Raw TCP/TLS** via `socket` module
- **Async networking** via `asyncio` streams
- **Third-party libraries** like `requests`, `httpx` (when loaded via `SandboxFactory`)

### `Callbacks`

Expose host functions to sandboxed code as async Python functions. Callbacks enable
controlled interaction between sandboxed code and the host environment.

#### Dict-Based API

Simple and explicit, good for dynamic callback registration:

```python
import eryx

def get_time():
    import time
    return {"timestamp": time.time()}

def fetch_user(user_id: int):
    # Call database, API, etc. from host
    return {"id": user_id, "name": "Alice", "email": "alice@example.com"}

sandbox = eryx.Sandbox(
    callbacks=[
        {
            "name": "get_time",
            "fn": get_time,
            "description": "Returns current Unix timestamp"
        },
        {
            "name": "fetch_user",
            "fn": fetch_user,
            "description": "Fetches user data from database"
        }
    ]
)

result = sandbox.execute("""
# Callbacks are available as async functions
t = await get_time()
print(f"Time: {t['timestamp']}")

user = await fetch_user(user_id=42)
print(f"User: {user['name']} ({user['email']})")
""")
```

#### Decorator-Based API

More Pythonic, uses `CallbackRegistry`:

```python
import eryx

registry = eryx.CallbackRegistry()

@registry.callback(description="Greets a person by name")
def greet(name: str, formal: bool = False):
    if formal:
        return {"greeting": f"Good day, {name}"}
    return {"greeting": f"Hey {name}!"}

@registry.callback(name="calc", description="Performs calculation")
def calculate(op: str, a: float, b: float):
    ops = {"add": a + b, "sub": a - b, "mul": a * b, "div": a / b}
    return {"result": ops[op]}

sandbox = eryx.Sandbox(callbacks=registry)

result = sandbox.execute("""
greeting = await greet(name="Alice", formal=True)
print(greeting['greeting'])

result = await calc(op="add", a=10, b=32)
print(f"10 + 32 = {result['result']}")
""")
```

#### Callback Requirements

- Must accept JSON-serializable arguments
- Must return JSON-serializable values (typically a dict)
- Can be sync or async (both work the same from Python's perspective)
- Are called as `await callback_name(...)` from sandboxed code

#### Discovering Callbacks

Sandboxed code can introspect available callbacks:

```python
result = sandbox.execute("""
import _callbacks
callbacks = _callbacks.list()
for cb in callbacks:
    print(f"{cb['name']}: {cb['description']}")
""")
```

#### Error Handling

Callbacks can raise exceptions that propagate to sandboxed code:

```python
def may_fail(should_fail: bool):
    if should_fail:
        raise ValueError("Operation failed!")
    return {"status": "ok"}

sandbox = eryx.Sandbox(
    callbacks=[{"name": "may_fail", "fn": may_fail, "description": "May fail"}]
)

result = sandbox.execute("""
try:
    await may_fail(should_fail=True)
except Exception as e:
    print(f"Caught: {e}")
""")
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

### `Session`

Unlike `Sandbox` which runs each execution in isolation, `Session` maintains
persistent Python state across multiple `execute()` calls. This is useful for:

- Interactive REPL-style execution
- Building up state incrementally
- Faster subsequent executions (no Python initialization overhead per call)

```python
import eryx

session = eryx.Session()

# State persists across executions
session.execute("x = 42")
session.execute("y = x * 2")
result = session.execute("print(f'{x} * 2 = {y}')")
print(result.stdout)  # "42 * 2 = 84"

# Functions and classes persist too
session.execute("""
def greet(name):
    return f"Hello, {name}!"
""")
result = session.execute("print(greet('World'))")
print(result.stdout)  # "Hello, World!"
```

#### Session with Virtual Filesystem

Sessions can optionally use a virtual filesystem (VFS) for persistent file
storage that survives across executions and even session resets:

```python
import eryx

# Create shared storage
storage = eryx.VfsStorage()

# Create session with VFS enabled
session = eryx.Session(vfs=storage)

# Write files to the virtual filesystem
session.execute("""
with open('/data/config.json', 'w') as f:
    f.write('{"setting": "value"}')
""")

# Files persist across executions
result = session.execute("""
import json
with open('/data/config.json') as f:
    config = json.load(f)
print(config['setting'])
""")
print(result.stdout)  # "value"

# Files even persist across session.reset()
session.reset()
result = session.execute("print(open('/data/config.json').read())")
# File still exists!
```

#### Sharing Storage Between Sessions

Multiple sessions can share the same `VfsStorage` for inter-session communication:

```python
import eryx

# Shared storage instance
storage = eryx.VfsStorage()

# Session 1 writes data
session1 = eryx.Session(vfs=storage)
session1.execute("open('/data/shared.txt', 'w').write('from session 1')")

# Session 2 reads it
session2 = eryx.Session(vfs=storage)
result = session2.execute("print(open('/data/shared.txt').read())")
print(result.stdout)  # "from session 1"
```

#### Custom Mount Path

By default, VFS files are accessible under `/data`. You can customize this:

```python
session = eryx.Session(vfs=storage, vfs_mount_path="/workspace")
session.execute("open('/workspace/file.txt', 'w').write('custom path')")
```

#### State Snapshots

Capture and restore Python state for checkpointing:

```python
session = eryx.Session()
session.execute("x = 42")
session.execute("data = [1, 2, 3]")

# Capture state as bytes (uses pickle internally)
snapshot = session.snapshot_state()

# Clear state
session.clear_state()

# Restore from snapshot
session.restore_state(snapshot)
result = session.execute("print(x, data)")
print(result.stdout)  # "42 [1, 2, 3]"

# Snapshots can be saved to disk and restored in new sessions
with open("state.bin", "wb") as f:
    f.write(snapshot)
```

#### Session Properties and Methods

- `session.execute(code)` - Execute code, returns `ExecuteResult`
- `session.reset()` - Reset Python state (VFS persists)
- `session.clear_state()` - Clear variables without full reset
- `session.snapshot_state()` - Capture state as bytes
- `session.restore_state(snapshot)` - Restore from snapshot
- `session.execution_count` - Number of executions performed
- `session.execution_timeout_ms` - Get/set timeout in milliseconds
- `session.vfs` - Get the `VfsStorage` (if enabled)
- `session.vfs_mount_path` - Get the VFS mount path (if enabled)

### `VfsStorage`

In-memory virtual filesystem storage. Files written to the VFS are completely
isolated from the host filesystem - sandboxed code cannot access real files.

```python
import eryx

# Create storage (can be shared across sessions)
storage = eryx.VfsStorage()

# Use with Session
session = eryx.Session(vfs=storage)
```

The VFS supports standard Python file operations:
- `open()`, `read()`, `write()` - File I/O
- `os.makedirs()`, `os.listdir()`, `os.remove()` - Directory operations
- `os.path.exists()`, `os.path.isfile()` - Path checks
- `pathlib.Path` - Full pathlib support

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
