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

The shell runtime is a WASI 0.3 component (`wasm32-wasip3`), and the Python and
JavaScript runtimes are WASI 0.2 ones; eryx links both versions of WASI, so
each sees the same sandboxed filesystem, environment and clocks. The JavaScript
package (`@eryx-org/eryx`) still runs a WASI 0.2 build of the shell.

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
`process.env`; the shell exports their placeholders to its commands (see
**Network** below). A shell script fails with its exit status
(`ErrorKind::ScriptFailed { exit_code: Some(n), .. }`; in Python,
`PythonExceptionError.exit_code`; in eryx-server's `ExecuteResult`, `exit_code`,
which is also 0 for a shell script that succeeds). Besides bash's builtins,
the shell has `grep` built in. Patterns are GNU basic regular expressions, as
in grep (`a\|b`, `\(x\)`), and `-E` and `-F` work as usual. Callbacks are commands (see
[Callbacks in the Shell](./callbacks.md#callbacks-in-the-shell)).

### Timers in JavaScript

JavaScript has `setTimeout`, `setInterval`, `setImmediate` and their `clear*`
functions, `queueMicrotask`, and `node:timers` and `node:timers/promises`
(`await setTimeout(ms, value, { signal })`, and `setInterval` as an async
iterator). Waiting burns no fuel, but it counts towards the execution
timeout.

- **An execute waits for referenced timers**, as a Node process does. So
  `setTimeout(() => console.log("hi"), 100)` prints `hi` without an `await`,
  and a `setInterval` that's never cleared runs until the execution timeout
  fails the execute (`ErrorKind::Timeout`). A timed-out execute discards its
  instance, so a session's next execute starts fresh (`state_was_reset`), as
  after running out of fuel.
- **It waits for callbacks started after the script's synchronous run**: in a
  promise callback (`Promise.resolve().then(() => tool())`), after an `await`,
  or in a timer. A callback the script's top-level code starts synchronously
  and never awaits (`tool(); console.log("done")`) is released when the
  script ends, as in Python.
- **`timer.unref()`** lets the execute finish without that timer, and an
  unref'd timer alone (like `AbortSignal.timeout()`'s) doesn't keep a script
  that never settles waiting. Timers still pending when the execute finishes
  are dropped: none carries over to a session's next execute.
- Each turn of the event loop runs the timers due when it started, then lets
  finished callbacks in, so timers that are always due (a `setImmediate`
  loop) don't starve callback results.
- An exception thrown in a timer callback fails the execute.
- `fetch` is synchronous inside the guest: it blocks until the response has
  been read, so timers don't fire during a fetch, only after it returns.

### Shell Environment

A shell sandbox starts with what CLI tools expect of a Unix system:

- **`/`** is a read-only directory holding the mounts, so tools that walk up
  to it (`mkdir -p`, canonicalizing `realpath`, git's repository discovery)
  work. Paths under a mount go to the mount; anything else under `/` doesn't
  exist (`cat /etc/passwd` fails with "No such file or directory"), and
  nothing can be created, written, renamed or removed there.
- **`/tmp`** is writable in every shell sandbox. With `unstable-vfs` it's
  served from the sandbox's VFS storage; without it, from a private in-memory
  storage. Either way it counts against `ResourceLimits::max_vfs_bytes`
  (default 64 MiB), and in a session it lasts until the session ends, across
  resets.
- **`/dev/null`**, **`/dev/zero`** and **`/dev/urandom`**: writes are
  discarded; reads are empty, zeros and random bytes. A single read returns at
  most 1 MiB, however much was asked for. A mount at `/dev` replaces them.
- **Environment:** `HOME=/tmp`, `PATH=/eryx/bin` (where the shell lists its
  commands), `TERM=dumb`, `LANG=C.UTF-8`, `LC_ALL=C.UTF-8`, `USER=eryx`,
  `PWD=/` and `TMPDIR=/tmp`.
- **Permissions:** WASI has no file modes, owners or exec bit, so what tools
  report is made up by their libc. uutils `ls -l` shows every directory as
  `dr-xr-xr-x` and every file (devices included) as `-r-xr-xr-x`, owned by
  `somebody`/`somegroup`, whatever can actually be written. Tools that compare
  modes against what they wrote need telling, for example git's
  `core.fileMode=false`.

Python and JavaScript sandboxes keep their filesystem as it was: no root,
`/dev` or these variables. The `shell` feature pulls in eryx-vfs to provide
this, but the VFS API stays behind `unstable-vfs`.

### External Commands

A shell sandbox can run WASI command programs as external commands: WASI
0.2 and 0.3 command components (built for `wasm32-wasip2` or
`wasm32-wasip3`, such as uutils coreutils, ripgrep or curl), and WASI preview
1 command modules (`wasm32-wasip1`, or any `wasi_snapshot_preview1` CLI),
which eryx adapts into 0.2 components when it loads them, with the adapter
of the wasmtime it links. A preview 1 command's exit status reaches the
shell only as success (0) or failure (1). Register them by name on a shell
builder:

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

- **Defaults:** with the `shell` feature, `Sandbox::embedded().shell()` already
  runs uutils coreutils (`cat`, `head`, `tail`, `ls`, `wc`, `cp`, `mv`, `rm`,
  `mkdir`, `touch`, `sort`, `seq`, `cut`, `tr`, `uniq`, `basename`, `dirname`,
  `tee`, `sleep`, `paste`, `comm`, `date`, `base64`, `realpath`, `readlink`,
  `rmdir`; `eryx::embedded::COREUTILS_TOOLS`). A registered command replaces
  the tool of the same name, and `without_embedded_commands()` leaves them out.
- **Python:** pass commands by name, e.g.
  `eryx.Sandbox(language="shell", commands={"rg": eryx.Command.from_file("rg.wasm")})`
  (`Command.from_bytes` and `Command.from_precompiled_file` too). The
  `pyeryx-runtime-shell` package (`pip install pyeryx[shell]`) brings
  coreutils, which pyeryx registers by default.
- **eryx-server:** `--shell-command NAME[,NAME...]=PATH[@sha256:<hex>]`
  (repeatable, or `ERYX_SHELL_COMMANDS` separated by `;`) registers a `.wasm`
  command or one precompiled by `eryx-precompile compile` (`.cwasm`). A
  `.wasm` can be pinned to its digest (see
  [Commands from a registry](./grpc-server.md#commands-from-a-registry)).
  `--no-shell-coreutils` leaves the embedded coreutils out, and
  `--shell-command-tunnels NAME` (repeatable) lets a command open tunnels
  (see **Network**). An empty `ERYX_SHELL_COMMANDS` means no extra commands.
- **JavaScript (`@eryx-org/eryx`):** a shell sandbox includes coreutils (as a
  download on first use; `coreutils: false` leaves it out), and `commands`
  adds others by the URL of their `jco transpile --instantiation async`
  output. They run in batch, not as a pipeline: each reads all its stdin,
  runs to completion, then passes on its output (see the package's README).

- **Per execution:** an execution can be narrowed to some of the registered
  commands: `with_allowed_commands(["cat", "sort"])` on a session's (or
  executor's) execute builder or on the `ExecuteOptions` of
  `Sandbox::execute_with`, `allowed_commands=[...]` on pyeryx
  `Sandbox.execute` and `Session.execute`, or
  `ExecuteRequest.allowed_commands` on eryx-server.
  The guest sees only those, and the others exit 127 as if not installed. An
  empty list allows none, unset allows all, and unknown names are ignored. It
  lasts one execution: a session's next call without it gets every command
  back. The shell's `grep` builtin counts as a command here (allow `grep` to
  keep it): left out, it fails as not installed when run, though `type grep`
  and `command -v grep` still report a builtin. Other builtins and callbacks
  aren't commands and keep working. In a
  session, a command an earlier call allowed still shows in the shell's
  `hash` table, but running it is refused.
- **Per command, per execution:** `with_command_options(name, options)` on
  the same builders (and `ExecuteOptions`) takes away from what one
  registered command gets in that execution, as a `CommandOptions`:
  `with_network(ChildNetwork::None | Inherit | Narrow(config))` gives its
  children no network, the execution's, or only what both `config` and the
  execution's allow (its connections and bytes count against `config`'s
  limits and the execution's); `without_tunnels()` and `without_spawn()` turn
  off what `allow_tunnels()` and `allow_spawn()` gave it at registration.
  Options never add: the execution fails to start if they name a command
  that isn't registered, or give one network when the sandbox gives
  children none. A command it spawns gets its own options, not its
  caller's, so pair `None` or `Narrow` with `without_spawn()` to stop it
  reaching a command with more network. Like `with_allowed_commands`,
  they last one execution.
- **Lookup:** the shell looks for functions first, then builtins, then
  callbacks, then registered commands. An unregistered name exits 127.
- **Running:** each command runs as a child process with `argv[0]` set to the
  name it was called by, which is how a multicall binary picks its tool. It gets
  the shell's environment, with `PWD` set to the shell's directory, which is
  also its WASI initial working directory. Children built with `eryx-build`
  start in that directory; other wasi-libc programs start in `/`, because
  wasi-libc ignores both (WebAssembly/wasi-libc#915).
- **What children get:** WASI 0.2.x and 0.3.x `wasi:cli`, `wasi:clocks`,
  `wasi:filesystem`, `wasi:random` and `wasi:sockets` (and 0.2's `wasi:io`),
  plus `eryx:process/run@0.1` for commands registered with `allow_spawn`.
  Nothing else: no `wasi:http` (children reach the network over sockets,
  through the proxy below), and no WASI prereleases. A command importing
  anything else is refused when it's loaded (`Command::from_file` and
  friends, or `eryx-precompile compile`), with each such import named. Check
  a CLI before registering it with `eryx-precompile commands inspect cli.wasm`
  (`--json` for a script; `Command::inspect` in Rust), which lists its
  imports and whether children get each, and exits 1 if eryx can't run it
  (2 if it can't read the file as a command at all).
- **Files:** a child sees the shell's filesystem (above): the read-only root,
  `/tmp`, `/dev` and, with `unstable-vfs`, the session's VFS mounts, and
  nothing else of the host. Files a child writes are the shell's, and last as
  long as the shell's own.
- **Network:** none by default. With the sandbox's network, children can
  get some through a proxy on the host, and never more than the sandbox's
  policy allows: in Rust `with_child_network(ChildNetwork::Inherit)` (that
  policy) or `ChildNetwork::Narrow(config)` (only what both allow); in
  Python `child_network="inherit"` or a `NetConfig`; in eryx-server
  `network_config.child_network`. A narrowing config starts from the
  defaults, which block localhost and private networks, so it must repeat
  any allowed networks or internal hosts its children need. Children get `HTTPS_PROXY`/`HTTP_PROXY`
  and the proxy's CA at `/etc/ssl/certs/ca-certificates.crt`, and see
  secrets only as their placeholders, which the proxy replaces in requests
  to each secret's hosts. A command allowed tunnels
  (`Command::allow_tunnels`, `allow_tunnels=True`, `--shell-command-tunnels`)
  can open blind `CONNECT` tunnels instead, for protocols the proxy can't
  inspect: no secret substitution and no HTTP rules in them, and hosts with
  rules never get one.
- **Limits:** children count against the execution's `max_fuel` as they
  run (each may burn at most 10 billion instructions of it), and stop at its
  `execution_timeout` and cancellation, even mid-computation. They're
  charged once per 10 ms epoch tick, so together they can overshoot
  `max_fuel` by at most one tick of work per running child (at most 64).
  The running children share 256 MiB of memory, outside `max_memory_bytes`;
  a child that can't grow fails like one out of memory. Children run on an
  engine of their own that allocates on demand, so they take none of the
  pooling allocator's slots (`ERYX_POOL_INSTANCES`).
  The host memory children's arguments and environment hold is capped per
  execution by
  `ResourceLimits::max_child_host_bytes` (default 64 MiB), with at most 64
  children spawned but not yet finished and 2 MiB of arguments and
  environment per spawn. Past a limit the spawn fails and the shell reports
  `<command>: limit-exceeded`.
- **Pipes:** a child's stdin, stdout and stderr stream through 64 KiB pipes,
  so pipeline stages run together and a full pipe makes its writer wait:
  `yes | head -n 1` stops once `head` exits. A loop of builtins writing into
  a pipe (`while true; do echo y; done | head -n 1`) still runs until its
  timeout, since the shell has no `SIGPIPE` to stop it.
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
