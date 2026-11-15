# Sandboxes

Sandboxes are the core primitive in Eryx. A sandbox provides an isolated Python execution environment powered by WebAssembly, ensuring complete isolation from the host system.

## Creating a Sandbox

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Create a sandbox with embedded runtime (recommended)
    let sandbox = Sandbox::embedded().build()?;

    // Execute code
    let result = sandbox.execute("print('Hello!')").await?;
    println!("{}", result.stdout_text());

    Ok(())
}
```

```python
import eryx

# Create a sandbox
sandbox = eryx.Sandbox()

# Execute code
result = sandbox.execute("print('Hello!')")
print(result.stdout_text)  # "Hello!"
```
<!-- langtabs-end -->

## Guest Languages

Python is the default guest language. Eryx can also run JavaScript (QuickJS) and
a bash-compatible shell (brush), through the same API: callbacks, resource
limits, sessions and snapshots work the same way. In Rust, enable the
`javascript` and `shell` cargo features to embed their runtimes, then switch
the builder's language. In Python, the pyeryx wheel only includes the Python
runtime: install the others with `pip install pyeryx[javascript]`,
`pyeryx[shell]` or `pyeryx[all]` (or pass `runtime_path=` to a precompiled
`.cwasm`).

<!-- langtabs-start -->
```rust,ignore
let js = Sandbox::embedded().javascript().build()?;
let result = js.execute("console.log(6 * 7)").await?;

let sh = Sandbox::embedded().shell().build()?;
let result = sh.execute("echo $((6 * 7))").await?;
```

```python
import eryx

js = eryx.Sandbox(language="javascript")
result = js.execute("console.log(6 * 7)")
```
<!-- langtabs-end -->

Python-only options, such as the stdlib, site-packages and packages, are only
available on a Python builder, as are library preambles. JavaScript has
[`fetch`](./networking.md#fetch-in-javascript) and reads secrets from
`process.env`; the shell can't have secrets. A shell script fails with its exit status
(`ErrorKind::ScriptFailed { exit_code: Some(n), .. }`). Besides bash's builtins,
the shell has `grep` built in. Patterns are GNU basic regular expressions, as
in grep (`a\|b`, `\(x\)`), and `-E` and `-F` work as usual. Callbacks are commands (see
[Callbacks in the Shell](./callbacks.md#callbacks-in-the-shell)).

### External Commands

A shell sandbox can run WASI 0.2 command components (programs built for
`wasm32-wasip2`, such as uutils coreutils, ripgrep or curl) as external
commands. Register them by name on a shell builder:

```rust,ignore
use eryx::{Command, Sandbox};

let coreutils = Command::from_file("coreutils.wasm")?; // compiled once
let rg = Command::from_file("rg.wasm")?;
let sandbox = Sandbox::embedded()
    .shell()
    .with_commands(["cat", "head", "sort", "wc"], coreutils) // one multicall binary
    .with_command("rg", rg)
    .build()?;
let result = sandbox.execute("printf 'b\na\n' | sort | head -n 1").await?;
```

- **Lookup:** the shell looks for functions first, then builtins, then
  callbacks, then registered commands. An unregistered name exits 127.
- **Running:** each command runs as a child process with `argv[0]` set to the
  name it was called by, which is how a multicall binary picks its tool. It gets
  the shell's environment, with `PWD` set to the shell's directory.
- **Files:** a child sees the session's VFS mounts (with `unstable-vfs`) and
  nothing else of the host.
- **Network:** children have none yet.
- **Limits:** children count against the execution's `max_fuel` (each
  may burn at most 10 billion instructions of it) and stop at its
  `execution_timeout` and cancellation, even mid-computation. Memory is per
  child: each may grow to 256 MiB, outside `max_memory_bytes`; children run
  one at a time.
  The host memory children hold (arguments, environment, buffered stdin and
  unread output) is capped per execution by
  `ResourceLimits::max_child_host_bytes` (default 64 MiB), with at most 64
  children spawned but not yet finished and 2 MiB of arguments and
  environment per spawn. Past a limit the spawn fails and the shell reports
  `<command>: limit-exceeded`.
- **Pipes:** children run in batch: a stage's input is buffered before it
  runs, so an endless producer piped into a command runs out of fuel.
- **Precompiling:** `Command::from_precompiled{,_file}` loads one precompiled
  with `Executor::precompile` or `eryx-precompile compile` (without
  `--language`). It carries the same version stamp as precompiled runtimes, so
  one from another eryx version fails with `ErrorKind::IncompatiblePrecompiled`.

The gRPC server serves every language it has a pool for (`--languages`), and
each request picks one with `ExecuteRequest.language`.

## Sandbox Isolation

Each sandbox runs in complete isolation:

- **Memory isolation**: No access to host memory
- **Filesystem isolation**: No access to host filesystem (see [VFS](./vfs.md) for virtual filesystem)
- **Network isolation**: Disabled by default (see [Networking](./networking.md) to enable)
- **Process isolation**: Cannot spawn processes or execute system commands

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    // Host filesystem is not accessible
    let result = sandbox.execute(r#"
import os
try:
    files = os.listdir('/etc')
    print(f"Found {len(files)} files")
except Exception as e:
    print(f"Access blocked: {type(e).__name__}")
    "#).await?;

    println!("{}", result.stdout_text());
    // Output: "Access blocked: FileNotFoundError" or shows empty virtual directory

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

# Host filesystem is not accessible
result = sandbox.execute("""
import os
try:
    files = os.listdir('/etc')
    print(f"Found {len(files)} files")
except Exception as e:
    print(f"Access blocked: {type(e).__name__}")
""")

print(result.stdout_text)
# Output: "Access blocked: FileNotFoundError" or shows empty virtual directory
```
<!-- langtabs-end -->

## Execution Results

When you execute code, you get back an `ExecuteResult` with useful information:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    let result = sandbox.execute("x = sum(range(1000))").await?;

    println!("stdout: {}", result.stdout_text());
    println!("Duration: {:?}", result.stats.duration);
    println!("Peak memory: {:?} bytes", result.stats.peak_memory_bytes);
    println!("Fuel consumed: {:?}", result.stats.fuel_consumed);

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

result = sandbox.execute("x = sum(range(1000))")

print(f"stdout: {result.stdout}")
print(f"Duration: {result.duration_ms}ms")
print(f"Peak memory: {result.peak_memory_bytes} bytes")
print(f"Callback invocations: {result.callback_invocations}")
```
<!-- langtabs-end -->

### Trace Collection

In Rust, the sandbox can also record line-level execution events in
`ExecuteResult::trace`. This is off by default because it installs Python's
`sys.settrace` hook, whose cost scales with how much Python runs: `pass`
costs about 20% extra per execution, a small `json` + `string.Template`
render goes from 0.9 ms to 3.7 ms, and `sum(i * i for i in range(20_000))`
from 3.8 ms to 369 ms. Enable it when you read the trace:

```rust
# extern crate eryx;
use eryx::Sandbox;

# fn main() -> Result<(), eryx::Error> {
let sandbox = Sandbox::embedded().with_trace_collection(true).build()?;
# Ok(())
# }
```

Each execution queues at most 16 MiB of trace events and 16 MiB of
streamed output for handlers, counting a fixed 64-byte overhead per event
or chunk. Past that, further events are dropped and `ExecuteResult::stream_truncated`
is set; the captured stdout/stderr in the result are complete. Direct fd
1/2 writes are captured separately, up to 1 MiB per stream, and streamed
once at the end of the execution, so a handler receives at most
16 MiB + 2 × 1 MiB of output per execution.

A `TraceHandler` set with `with_trace_handler` still receives events when
collection is disabled. Sessions created from the sandbox inherit the setting.

The Python bindings never collect traces (`ExecuteResult` does not expose
them), so no option is needed there.

## Error Handling

Sandbox execution can fail for various reasons. Eryx provides typed errors to help you handle them:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, ErrorKind};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    match sandbox.execute("raise ValueError('oops')").await.map_err(eryx::Error::into_kind) {
        Ok(result) => println!("Success: {}", result.stdout_text()),
        Err(ErrorKind::Execution(message)) => {
            println!("Python error: {}", message);
        }
        Err(ErrorKind::Timeout(duration)) => {
            println!("Execution timed out after {:?}", duration);
        }
        Err(e) => println!("Other error: {}", e),
    }

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

try:
    sandbox.execute("raise ValueError('oops')")
except eryx.ExecutionError as e:
    print(f"Python error: {e}")
except eryx.TimeoutError as e:
    print(f"Timeout: {e}")
except eryx.EryxError as e:
    print(f"Other error: {e}")
```
<!-- langtabs-end -->

## Reusing Sandboxes

Sandboxes can be reused for multiple executions. Each execution starts fresh without any state from previous executions:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    // First execution
    sandbox.execute("x = 42").await?;

    // Second execution - x is NOT available (fresh state each time)
    let result = sandbox.execute(r#"
try:
    print(x)
except NameError:
    print("x is not defined")
    "#).await?;

    println!("{}", result.stdout_text());  // "x is not defined"

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

# First execution
sandbox.execute("x = 42")

# Second execution - x is NOT available (fresh state each time)
result = sandbox.execute("""
try:
    print(x)
except NameError:
    print("x is not defined")
""")

print(result.stdout_text)  # "x is not defined"
```
<!-- langtabs-end -->

If you need state to persist across executions, use a [Session](./sessions.md).

## SandboxFactory for Fast Creation

When creating many sandboxes, use `SandboxFactory` to pre-initialize Python and packages once, then quickly instantiate sandboxes from that snapshot. Executions on short-lived sandboxes also pick up a pre-instantiated WebAssembly instance from a process-wide pool; see [Performance Tuning](./performance.md) for how that works and how to size it.

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
// SandboxFactory is primarily useful in the Python bindings
// In Rust, use Sandbox::embedded() which already uses pre-compiled WASM
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Sandbox::embedded() is already optimized for fast creation
    for i in 0..5 {
        let sandbox = Sandbox::embedded().build()?;
        let result = sandbox.execute(&format!("print('sandbox {}')", i)).await?;
        println!("{}", result.stdout_text());
    }
    Ok(())
}
```

```python
import eryx

# Create a factory (takes ~2s to initialize Python)
factory = eryx.SandboxFactory()

print(f"Factory size: {factory.size_bytes} bytes")

# Create sandboxes quickly from the factory (~1ms each)
for i in range(5):
    sandbox = factory.create_sandbox()
    result = sandbox.execute(f"print('sandbox {i}')")
    print(result.stdout_text)
```
<!-- langtabs-end -->

### Factory with Pre-installed Packages

You can create a factory with packages pre-installed:

```python
import eryx

# Create a factory with packages
factory = eryx.SandboxFactory(
    packages=[
        "path/to/jinja2.whl",
        "path/to/markupsafe.whl",
    ],
    imports=["jinja2"],  # Pre-import these modules
)

# Sandboxes created from this factory have packages ready
sandbox = factory.create_sandbox()
result = sandbox.execute("""
from jinja2 import Template
t = Template("Hello {{ name }}")
print(t.render(name="World"))
""")
print(result.stdout_text)  # "Hello World"
```

### Saving and Loading Factories

Factories can be serialized for later use:

```python
import eryx
from pathlib import Path

# Create and save
factory = eryx.SandboxFactory()
factory.save(Path("factory.bin"))

# Load later
loaded = eryx.SandboxFactory.load(Path("factory.bin"))
sandbox = loaded.create_sandbox()
```

## Next Steps

- [Sessions](./sessions.md) - Maintain state across executions
- [Resource Limits](./resource-limits.md) - Control execution time and memory
- [Callbacks](./callbacks.md) - Allow sandbox code to call host functions
- [Networking](./networking.md) - Enable network access
