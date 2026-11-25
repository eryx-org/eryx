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

`sh` and `bash` run a nested shell in the same guest: `bash -c 'CMD' [NAME
[ARG]...]`, `sh FILE [ARG]...`, or commands from stdin (`echo 'echo hi' |
bash`, `bash -s ARG`), with `-e`, `-u`, `-x`, `-o OPTION` and their `+`
forms. A nested shell behaves like an external command. It sees only exported
variables, its own file descriptors and the working directory. Nothing it
changes (variables, functions, options, `cd`) comes back. Its exit status is
the command's. It shares the sandbox's filesystem, commands and callbacks.
Shells nest at most 16 deep; past that `sh`/`bash` exits 126. Like bash's,
a nested shell doesn't wait for its background jobs: they keep running after
it, and the script's implicit `wait` (below) covers them.

Any file is runnable (`./script.sh`, or a file in a `$PATH` directory). WASI
has no execute bit. A file whose first line is `#!/bin/sh`, `#!/bin/bash`,
`#!/usr/bin/env bash` (or `dash`) runs in a nested shell, with the line's one
argument if it has one (`#!/bin/bash -e`). Any other `#!` line runs the
sandbox's command of the interpreter's file name, with the script's path:
`#!/usr/bin/env python3` runs `python3 ./script`. A command that isn't there
exits 127. A file without `#!` runs as an `sh` script. A path always
names a file, never a command: `./ls` runs a file named `ls`, not the `ls`
command. `env -S` in a `#!` line isn't supported. `chmod` is a builtin that
changes nothing (`ls -l` and `stat` show the same mode after it): it checks
the mode (`+x`, `u+x`, `755`, ...) and that each file exists, so
`chmod +x script.sh && ./script.sh` works. A missing file fails as in GNU
`chmod` (exit 1, `chmod: cannot access ...`).

`env` is a builtin, because uutils' `env` can't run commands on WASI. `env`
alone prints the exported environment. `env [-i] [-u NAME]... [NAME=VALUE]...
COMMAND [ARG]...` runs `COMMAND` with those variables added (`NAME=VALUE`),
removed (`-u`), or as the only ones (`-i`). The command is looked up like any
other (builtins, commands, `sh`/`bash`, scripts) except functions. A command
that isn't there exits 127, and any other option exits 125.

`xargs` is a builtin too, for the same reason, so its command is looked up
like `env`'s: `ls *.py | xargs grep -l TODO` runs the
`grep` builtin, and `xargs -I {} sh -c '...'` a nested shell. It takes GNU's
options: `-0`/`--null`, `-d DELIM`, `-a FILE`, `-n MAX`, `-L MAX`, `-I
REPLACE` (and `-i`), `-r`, `-s MAX`, `-x`, `-t`, `-E EOF` and `-P MAX`, with
GNU's quoting (`'a b'`, `"a b"`, `a\ b`) when items are split at blanks. The
default command is `echo`. Command lines are also kept under the host's limit
on a command's arguments and environment (2 MiB), so long input runs as
several commands, never one that fails to start. `-P` runs commands at once,
32 at most, and `-P 0` means 8. Commands running at once share the
execution's child memory pool (`max_child_memory_bytes`, 256 MiB by
default), so a run that doesn't fit fails (with memory-heavy commands such
as `python3`, keep `-P` low). `xargs` exits 123 if a command failed
(exit 1-125), 124 if one exited 255, 125 if one was killed (exit 129-254,
as a job `kill` stops exits), 126 or 127 if the command can't be run or
isn't there, and 1 for its own errors. It reads all its input before running
the first command, so it doesn't work on endless input (`yes | xargs`). The
commands get its stdin, which it has read to the end (or, with `-a`, left
alone). `-p` isn't supported.

`timeout` is a builtin too (uutils' needs processes and signals), so
`timeout 30 pytest` runs `pytest` looked up like `env`'s command. It takes
GNU's arguments, `timeout [OPTION] DURATION COMMAND [ARG]...`: `DURATION` is a
number of seconds with an optional `s`, `m`, `h` or `d` suffix, and `0` means
no limit. When the time runs out, the command is stopped at once, as `kill`
stops a job (with its builtins and child commands), and `timeout` exits 124.
`--preserve-status` makes that 128 plus the signal's number (`-s SIGNAL`, TERM
by default). `--foreground` and `-k`/`--kill-after` are accepted, and change
nothing. Otherwise it exits with the command's status: 127 if it isn't there,
126 if it can't be run, and 125 if `timeout` itself fails. The shell's
builtins run in the guest, between waits on child commands and pipes, so a
command that only loops on builtins without waiting (`sh -c 'while :; do :;
done'`) isn't stopped by `timeout`; the execution's timeout still ends it.

Background jobs work as in bash: `cmd &` runs `cmd` alongside the script, and
`$!` is its ID. `wait` waits for every job, `wait ID|%N...` for those jobs
(returning the last one's status), and `wait -n` for the first to finish (127
if there are none). `jobs` (with `-l` or `-p`) lists them. `kill [-SIGNAL]
ID|%N...` stops a job, its builtins and its child commands. WASI has no signals,
so every signal ends the job, which then exits with 128 plus the signal's number
(143 for the default `TERM`). `kill -0` checks that a job is still running.
Job IDs are the shell's own; no process has one. A script ends with an
implicit `wait`: jobs still running then finish within the same execution,
their output is its output, and its status is still the script's own. If the
execution would time out while only jobs are left (the script itself has
finished), the jobs are killed just before the deadline, stderr gets one line
saying how many (`eryx: killed 2 background jobs still running when the
execution timed out`), and the execution returns the script's result instead
of a timeout. Jobs a nested shell left running count too. A script that is itself still running at the timeout times out
as usual. A job that loops on builtins alone (`while :; do :; done &`, with no
command, pipe or `sleep` in it) never lets the shell stop it in time, so it
ends in a timeout too. In a session, jobs never outlive the execution that
started them. Jobs count against the execution's limits like any other
command. `fg`, `bg`, stopped jobs, `trap` on signals, and `%name` job specs
aren't supported.

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
- **It waits for every callback in flight**, awaited or not, wherever it
  started: at top level, in a promise callback, after an `await`, or in a
  timer. So a forgotten `await` (`tool(); console.log("done")`) still runs
  the callback to the end, and if it fails, the execute fails as an unhandled
  rejection (below). The wait counts towards the execution timeout. Python
  differs: a coroutine that's never awaited never runs, and tasks still
  pending when the script ends are dropped.
- **`timer.unref()`** lets the execute finish without that timer, and an
  unref'd timer alone (like `AbortSignal.timeout()`'s) doesn't keep a script
  that never settles waiting. Timers still pending when the execute finishes
  are dropped: none carries over to a session's next execute.
- Each turn of the event loop runs the timers due when it started, then lets
  finished callbacks in, so timers that are always due (a `setImmediate`
  loop) don't starve callback results.
- An exception thrown in a timer callback fails the execute.
- An uncaught error (or rejection, below) ends stderr as well as being the
  failure's message, as Python's traceback does.
- **A promise rejection still unhandled when the event loop drains fails the
  execute** as a script failure, with `Uncaught (in promise) Error: ...` and
  its stack as the message (the first one, with a count of any others). So
  does an async function called without `await` that throws. A rejection
  that gets a handler later, in a later microtask or timer, doesn't: this is
  more lenient than Node, which fails at the end of the tick. A rejection
  left over from a failed execute never fails a session's next one.
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
  `PWD=/` and `TMPDIR=/tmp`, plus any [environment
  variables](#environment-variables) you give it.
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
shell only as success (0) or failure (1). [Building Commands](./building-commands.md)
walks through building one. Register them by name on a shell builder:

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
  `rmdir`, `md5sum`, `sha1sum`, `sha256sum`, `sha512sum`, `b2sum`,
  `printenv`; `eryx::embedded::COREUTILS_TOOLS`). A registered command replaces
  the tool of the same name, and `without_embedded_commands()` leaves them out.
  It also registers itself as `sh` and `bash` for commands that run them (see
  **Shell as a command**).
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
  `--no-shell-embedded-commands` leaves the embedded coreutils (and `sh`/`bash`
  for children) out,
  `--shell-python` adds `python`/`python3` (see **Python as a command**),
  `--shell-node` adds `node`/`js` (see **JavaScript as a command**), and
  `--shell-command-tunnels NAME` (repeatable) lets a command open tunnels
  (see **Network**). An empty `ERYX_SHELL_COMMANDS` means no extra commands.
- **Python as a command:** `Command::guest(Language::Python)` (Rust, with
  `embedded`; `Command::guest_from_precompiled_file` for your own runtime),
  `eryx.Command.python()` (pyeryx) and `--shell-python` (eryx-server) run
  Python as a command. It isn't registered by default; register it as
  `python` and `python3`:

  ```rust,ignore
  let sandbox = Sandbox::embedded()
      .shell()
      .with_commands(["python", "python3"], Command::guest(Language::Python)?)
      .build()?;
  sandbox.execute("seq 3 | python3 -c 'import sys; print(sum(map(int, sys.stdin)))'").await?;
  ```

  It takes `-c CODE`, `-m MODULE`, a script, `-` or no argument (the program
  from stdin), `-V`, and ignores `-u`, `-B`, `-I`, `-E`, `-s` and `-q`;
  anything else exits 2. `sys.exit(n)` is its exit status, and an uncaught
  exception prints its traceback and exits 1. Each run is a fresh
  interpreter, with the embedded standard library, on the shell's files
  (`/tmp` and volumes), environment, working directory and stdio. It burns
  the execution's fuel, stops at its timeout, and its memory (about 17 MB
  per run, more as it allocates) counts against the children's shared
  pool (`max_child_memory_bytes`, 256 MiB by default). It gets no callbacks and no network (yet), so a credential can't
  be for it. A script's top-level `asyncio.run(main())` works as in
  `execute`.
- **Shell as a command:** a command registered with `allow_spawn` that runs
  `sh` or `bash` (git does, for aliases, hooks, `core.pager`,
  `GIT_SSH_COMMAND` and credential helpers) gets the embedded shell, as
  `Command::guest(Language::Shell)`: a fresh shell guest for each run, with
  `-c CMD [NAME [ARG]...]`, a script file or commands from stdin, the options
  above, and the args, env, directory and stdio the command gave it. It runs
  the commands its caller may run (the execution's allowed ones), one level
  deeper (it counts towards the depth limit of 4), within its caller's
  network, and has no callbacks and no network of its own. It waits for its
  background jobs before it exits, stopping them just before the
  execution's timeout. Each run costs about 0.1 ms and 5 MB of the children's
  memory pool. The embedded shell registers it unless
  `without_embedded_commands()`; an execution's allowed commands must name
  `sh` (or `bash`) for a child to run it, as for the shell's own nested `sh`.
- **JavaScript as a command:** `Command::guest(Language::JavaScript)` (Rust,
  with `embedded` and `javascript`), `eryx.Command.node()` (pyeryx, from the
  JavaScript runtime `Sandbox(language="javascript")` would use) and
  `--shell-node` (eryx-server) run QuickJS as a command. It isn't registered
  by default; register it as `node` and `js`:

  ```rust,ignore
  let sandbox = Sandbox::embedded()
      .shell()
      .with_commands(["node", "js"], Command::guest(Language::JavaScript)?)
      .build()?;
  sandbox.execute("seq 3 | node -e 'console.log(require(\"fs\").readFileSync(0, \"utf8\"))'").await?;
  ```

  It takes `-e CODE`, `-p CODE` (prints the value), a script (also through
  a `#!/usr/bin/env node` line), `-` or no argument (the program from
  stdin), `-v` (which prints the QuickJS version: this isn't Node) and
  `-h`; any other option exits 9, as in Node. The program runs as an
  `execute`'s code does (leading `import`s and top-level `await` work), with
  `process.argv` (`[execPath, script, ...args]`), `process.env`,
  `process.cwd()`, `process.exit(n)`, `process.exitCode` and
  `process.stdout.write`, and the main module's `require`, `module`,
  `exports`, `__filename` and `__dirname`. `require("./x")` loads a
  CommonJS module (or `.json`) from the shell's files, and a script's
  `import ... from "./x.mjs"` an ES module. `node:fs` reads and writes the
  shell's files: whole-file `readFileSync`/`writeFileSync`/`appendFileSync`
  (`readFileSync(0)` is stdin), `existsSync`, `statSync`, `readdirSync`,
  `mkdirSync`, `rmSync`, `renameSync`, `copyFileSync` and friends, each also
  as a callback and in `fs.promises`. There's no `process.stdin` stream,
  `fs.createReadStream` or file descriptor API. The command waits for its
  timers and un-awaited promises, and an uncaught error or unhandled
  rejection prints to stderr and exits 1. `process.exit` throws to unwind
  the script, so a `catch` around it sees that, but the command still exits
  with its status when that code returns. Limits, files and the missing
  callbacks and network are as for Python.
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
  back. The shell's `grep` builtin and its nested `sh` and `bash` count as
  commands here (allow `grep`, `sh` or `bash` to keep them): left out, they
  fail as not installed when run, though `type grep` and `command -v grep`
  still report a builtin. Other builtins and callbacks
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
- **A command's own environment and files:** in Rust,
  `Command::with_env(key, value)` gives a command a default for one
  variable, which a value its caller sets (`GIT_PAGER=less git log`, or an
  `export`) replaces; a caller can't unset a default, only set it (to an
  empty value, say). `Command::with_mount(volume)` (with `unstable-vfs`)
  mounts a `VolumeMount` in that command's filesystem and its children's,
  not the shell's or other commands': read-only unless the volume is
  writable, and a storage volume works as for a sandbox. A mount replaces
  what its parent has at the same path (a sandbox volume, or the same
  mount from the command that ran it); one beneath another mount shows
  through it. Commands that run each other and need the same files each
  get the mount:

  ```rust,ignore
  let goroot = VolumeMount::read_only("/opt/go", "/usr/local/go");
  let go = |cmd: Command| {
      cmd.with_mount(goroot.clone())
          .with_env("GOROOT", "/usr/local/go")
          .with_env("GOCACHE", "/tmp/go-cache")
  };
  let sandbox = Sandbox::embedded()
      .shell()
      .with_command("go", go(Command::from_file("go.wasm")?.allow_spawn()))
      .with_command("go-compile", go(Command::from_file("compile.wasm")?))
      .with_command(
          "git",
          Command::from_file("git.wasm")?
              .allow_spawn()
              .with_env("GIT_CONFIG_NOSYSTEM", "1")
              .with_env("GIT_TERMINAL_PROMPT", "0"),
      )
      .build()?;
  ```

  A host directory is opened each time the command runs; if that fails,
  the command fails to start, with the reason on its stderr.
- **What children get:** WASI 0.2.x and 0.3.x `wasi:cli`, `wasi:clocks`,
  `wasi:filesystem`, `wasi:random` and `wasi:sockets` (and 0.2's `wasi:io`),
  plus `eryx:process/run@0.1` and `eryx:process/pipe@0.1` for commands
  registered with `allow_spawn`: `run` runs a command to completion, and
  `pipe` starts one with live pipes to its stdin and stdout (as git talks to
  its `remote-https` helper). A piped command runs only while its caller is
  in a pipe call (writing, reading or waiting on any of its piped commands),
  so a caller blocked on something else that depends on it waits until the
  timeout.
  Nothing else: no `wasi:http` (children reach the network over sockets,
  through the proxy below), and no WASI prereleases. A command importing
  anything else is refused when it's loaded (`Command::from_file` and
  friends, or `eryx-precompile compile`), with each such import named. Check
  a CLI before registering it with `eryx-precompile commands inspect cli.wasm`
  (`--json` for a script; `Command::inspect` in Rust), which lists its
  imports and whether children get each, and exits 1 if eryx can't run it
  (2 if it can't read the file as a command at all).
- **Files:** a child sees the shell's filesystem (above): the read-only root,
  `/tmp`, `/dev` and, with `unstable-vfs`, the session's VFS mounts (plus
  its command's own mounts, above), and nothing else of the host. Files a
  child writes are the shell's, and last as long as the shell's own.
- **Network:** none by default. A child without network that tries to
  resolve a name or open a socket gets `eryx: networking not enabled for
  commands in this sandbox (curl has no network)` on its stderr (`for this
  command` when only its command's network is taken away), once, before
  its own error. With the sandbox's network, children can
  get some through a proxy on the host, and never more than the sandbox's
  policy allows: in Rust `with_child_network(ChildNetwork::Inherit)` (that
  policy) or `ChildNetwork::Narrow(config)` (only what both allow); in
  Python `child_network="inherit"` or a `NetConfig`; in eryx-server
  `network_config.child_network`. A narrowing config starts from the
  defaults, which block localhost and private networks, so it must repeat
  any allowed networks or internal hosts its children need. In Rust, a
  command can have less from registration on, with
  `Command::with_network(ChildNetwork::None | Inherit | Narrow(config))`
  (the same narrowing, in every execution and session call); an execution's
  `CommandOptions::with_network` replaces it for that execution. A command
  run by another (`allow_spawn`) gets only what both its own network and
  its caller's allow, so narrowing a command holds everything it runs too.
  `build`
  fails if a command is registered with network when the sandbox gives
  children none, or with an invalid narrowing config. Children get `HTTPS_PROXY`/`HTTP_PROXY`
  and the proxy's CA at `/etc/ssl/certs/ca-certificates.crt`, and see
  secrets only as their placeholders, which the proxy replaces in requests
  to each secret's hosts. A command allowed tunnels
  (`Command::allow_tunnels`, `allow_tunnels=True`, `--shell-command-tunnels`)
  can open blind `CONNECT` tunnels instead, for protocols the proxy can't
  inspect: no secret substitution and no HTTP rules in them, and hosts with
  rules never get one.
- **Credentials:** a `Credential` is a header the proxy adds to some
  commands' requests to some hosts, from a secret the sandbox never sees,
  not even as a placeholder, so `git` and `gh` authenticate with no token in
  their env or files:

  ```rust,ignore
  Sandbox::embedded().shell()
      .with_command("git", git.allow_spawn())
      .with_network(network)
      .with_child_network(ChildNetwork::Inherit)
      .with_credential(
          Credential::basic("x-access-token", token)
              .for_hosts(["github.com"])
              .for_paths(["/my-org/"])
              .for_commands(["git"]),
      )
  ```

  `bearer`, `token` (`Authorization: token …`), `basic(user, …)` and
  `header(name, template, …)` build the value. `for_hosts` is required, and
  takes exact names, IP addresses, or `*.` and a name of two labels or more
  (`*.github.com`, not `*.com`), no other `*`, port, path or non-ASCII
  character (write punycode); it doesn't scope by port. `for_paths` and `for_commands` narrow it (with
  no `for_commands`, every child gets it). Give one execution its own with
  `ExecuteOptions::with_credential` (a hosted service's per-user token): where
  it and a sandbox's both set a header on a request, the execution's wins.
  Take them from one command with `CommandOptions::without_credentials()`.
  - **Where:** only on requests the proxy inspects: over the TLS it
    terminates, or plain HTTP with `allow_plaintext()`. A connection to a
    credential's host refuses `Upgrade` and `CONNECT` requests outright, so
    those hosts can't be used for WebSockets or other upgrades. Any header of the same name the child sent is replaced, so `gh`
    can run with a dummy `GH_TOKEN`. A command opening blind tunnels can't
    carry one: naming it fails the execution, and a tunnelling proxy refuses
    a credential's hosts.
  - **Confined to its commands:** a command with credentials gets a proxy of
    its own, which wants a password from its `HTTPS_PROXY` URL (scrubbed
    from the output like the credential), and no
    connection, the sandbox's or a child's, may reach a child proxy, even
    with `allow_localhost`. Other commands and the script's own requests
    never get the header.
  - **Responses:** on a connection with a credential, requests ask for
    `Accept-Encoding: identity` and a compressed or transfer-coded response
    ends the connection, since the credential can't be found in an encoded
    body, as does any `101`. `Range` and `If-Range` are dropped from every
    request on such a connection, whether it gets the credential or not. The
    value (and for `Basic` its base64 payload, and the secret) is
    masked out as `[REDACTED]`, as secrets' real values are, and scrubbed
    from stdout, stderr and traces too in case any reaches them. Masking
    only hides whole, verbatim echoes: one in another form (its base64
    alone, hex, JSON's `\/` for `/`) or in pieces isn't masked, so don't give
    credentials for hosts that echo request headers. Output scrubbing is
    defence in depth, not a boundary (see
    [Credentials](./secrets.md#credentials-for-child-processes)).
  - **Limits:** a command with a credential can do anything with it its hosts
    and paths allow, whatever the script asks: scope it as narrowly as you
    can. Credentials are Rust-only for now.
- **Limits:** children count against the execution's `max_fuel` as they
  run (a child may burn all the fuel the execution has left, so with no
  `max_fuel` only the timeout bounds it), and stop at its
  `execution_timeout` and cancellation, even mid-computation. They're
  charged once per 10 ms epoch tick, so together they can overshoot
  `max_fuel` by at most one tick of work per running child (at most 64).
  The running children share one pool of memory,
  `ResourceLimits::max_child_memory_bytes` (default 256 MiB), outside
  `max_memory_bytes`; a child that can't grow fails like one out of memory,
  and one that can't start says so on the shell's stderr (`eryx: not enough
  memory for <command> (child memory pool full: ...)`). A child holds at
  most 16,384 resource handles (open files, streams, sockets) at once. Children run on an
  engine of their own that allocates on demand, so they take none of the
  pooling allocator's slots (`ERYX_POOL_INSTANCES`).
  The host memory children's arguments and environment hold is capped per
  execution by
  `ResourceLimits::max_child_host_bytes` (default 64 MiB), with at most 64
  children spawned but not yet finished and 2 MiB of arguments and
  environment per spawn. Each argument and variable counts its host
  overhead as well as its text (24 bytes per string on 64-bit hosts). Past a limit the spawn fails and the shell reports
  `<command>: limit-exceeded`, after a line saying which limit (`eryx: too
  many commands running (limit 64)`, say).
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

## Environment Variables

`with_env` (`env=` in Python, `ExecuteRequest.env` on the [gRPC
server](./grpc-server.md)) gives every execution ordinary configuration, such
as `GIT_AUTHOR_NAME` or a tool's config path, without writing it into the
code. Python reads it from `os.environ` and JavaScript from `process.env`; the
shell exports it, so its commands inherit it.

<!-- langtabs-start -->
```rust,ignore
let sandbox = Sandbox::embedded()
    .shell()
    .with_env("GIT_AUTHOR_NAME", "Eryx Bot")
    .with_env("GIT_AUTHOR_EMAIL", "bot@example.com")
    .build()?;
let result = sandbox.execute("printenv GIT_AUTHOR_NAME").await?;
```

```python
import eryx

sandbox = eryx.Sandbox(env={"GIT_AUTHOR_NAME": "Eryx Bot"})
result = sandbox.execute("import os; print(os.environ['GIT_AUTHOR_NAME'])")
```
<!-- langtabs-end -->

- Names must match `[A-Za-z_][A-Za-z0-9_]*`, and can't be one the sandbox
  sets itself (`HOME`, `PATH`, `PWD`, `USER`, `TMPDIR`, `TERM`, `LANG`,
  `LC_ALL`), one the shell manages, which it couldn't set (any `BASH*`,
  `UID`, `EUID`, `PPID`, `RANDOM`, `SRANDOM`, `SECONDS`, `LINENO`,
  `SHELLOPTS`, `DIRSTACK`, `FUNCNAME`, `GROUPS`, `HISTCMD`, `PIPESTATUS`,
  `EPOCHSECONDS`, `EPOCHREALTIME`), or a [secret](./secrets.md)'s name. A value can hold anything
  but a NUL byte: quotes and newlines arrive as they were given, never as
  code. The same names are refused in every language, and building the
  sandbox fails otherwise (`ErrorKind::Initialization`; `InitializationError`
  in Python).
- Values aren't secrets: they aren't scrubbed, so printing one prints it.
  Put credentials in [secrets](./secrets.md).
- They're set before each execution. Python and shell code can change them;
  JavaScript's `process.env` is frozen, as it is with only secrets.

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
