# Threat model

## What this project does and where untrusted input enters

Eryx runs untrusted Python, JavaScript and shell scripts inside a
WebAssembly sandbox. Each language has its own guest component (CPython 3.14,
QuickJS and the brush shell, compiled to WASM), executed with Wasmtime. The
host (the application embedding eryx) gives the guest capabilities
explicitly: async callbacks, a virtual filesystem, policy-controlled TCP/TLS,
secrets that the guest only ever sees as placeholders, and, for the shell
guest, child processes running commands the host registered.

**Guest code is hostile.** Assume an attacker controls the full text of
every script passed to `execute()`, and anything it computes at runtime:
callback arguments, stdout/stderr bytes, file contents and offsets written to
the VFS, hostnames and ports it connects to, bytes it sends, the values it
returns, and the argv, environment, stdin and files it gives child
processes. The attacker cannot change the host's configuration.

Untrusted input reaches the host through:

- **The guest↔host boundary.** These are the WIT imports in
  `crates/eryx-runtime/wit/` and their host implementations:
  callbacks (`crates/eryx/src/callback*.rs`), networking
  (`crates/eryx/src/net.rs`), the VFS (`crates/eryx-vfs/`), output and trace
  reporting (`crates/eryx/src/trace.rs`, `wasm.rs`), and secret substitution
  (`crates/eryx/src/secrets.rs`).
- **Child processes.** The `eryx:process` spawn, run and pipe imports
  (`crates/eryx/src/process.rs`, `process/`), the child HTTP(S) proxy with
  its per-sandbox CA (`crates/eryx/src/net/proxy.rs`, `policy.rs`,
  `credential.rs`, `response.rs`), and the VFS forks children share with the
  shell (`crates/eryx-vfs/`). See [Child processes](#child-processes).
- **Bytes the host decodes that the guest produced.** This covers session
  snapshots (`crates/eryx/src/session/`) and callback replay journals
  (`crates/eryx/src/replay.rs`).
- **`eryx-server` (gRPC).** Request fields and client-supplied snapshots and
  journals (`crates/eryx-server/src/`). The server is meant to sit behind a
  trusted backend, not to face the internet directly. Treat request *code* as
  hostile, and request *policy* fields (limits, network allowlists) as set by
  that trusted backend.

Trusted, and therefore not attacker-controlled: the embedding application, its
`Sandbox`/`SandboxBuilder` configuration, callbacks it registers, packages and
wheels it chooses to load, environment packs it builds, the precompiled
runtime `.cwasm` files and everything in `crates/eryx-runtime/libs/` and
`prebuilt/`. The *code* of registered commands is trusted to be what the
operator picked (see below), as is eryx's private cache directory, which holds
compiled commands.

## Child processes

The shell guest can run commands: WASI 0.2/0.3 command components or WASI
preview 1 modules that the host registers by name. Each run is a child
process with its own Wasmtime store. Commands with `allow_spawn` can run
other registered commands themselves (nested spawn).

There are two attacker models here:

- **An untrusted script** (the guest above) runs any command the execution
  allows, with any argv, env, stdin and files, as often as its limits let it.
- **A malicious command component.** A registered command's code is the
  operator's choice, but it may come from a third party, and commands that
  interpret their input (git config and aliases, interpreters) can be made
  to run attacker logic. So treat a child's behaviour, including its WASI
  calls and `eryx:process` imports, as hostile too. What it does with the
  capabilities it was granted (its argv, the filesystem, its network) is
  out of scope; reaching past them is not.

Children are held to the same boundary as guests:

- **What can run.** Children run only commands registered with the sandbox
  and allowed for the execution; names resolve against that map, never a
  path. Nested spawn needs `allow_spawn`, sees only the execution's allowed
  commands, and is limited to a nesting depth of 4 (the shell's children
  are the first level).
- **Guest runtimes as commands.** `Command::guest` runs an eryx guest
  runtime as a child through its `run-command` export: `python3` (opt-in)
  and `sh`/`bash` (registered by default in the embedded shell, for
  commands with `allow_spawn`). A guest child gets no callbacks and no
  network of its own, and the same budgets as any child. A shell child
  links the full `eryx:process` interface (spawn, wait, pipe), but with its
  spawner's table, one level deeper, and its children held within its
  spawner's network, so `sh` gives a malicious command nothing it couldn't
  run directly. Its argument parsing and the host's `eryx:process` imports
  reached from a child store are in scope.
- **Budgets charged to the parent.** All of an execution's children, nested
  and piped ones included, share its fuel and timeout, and run inside the
  parent's wait, so ending the execution ends them. They also share:
  - a child memory pool (`max_child_memory_bytes`, default 256 MiB,
    configurable by the embedder and, under the server's
    `--max-child-memory-bytes` ceiling, per gRPC request) covering each
    child's linear memory, tables and instance metadata (each core instance a
    command component creates, estimated before it runs). It is on top of the
    guest's `max_memory_bytes`, so an operator raising it raises the host
    memory one execution can hold;
  - a host-bytes budget (`max_child_host_bytes`, default 64 MiB) charged for
    each spawn's argv and env, including per-argument overhead, for captured
    output and for trace copies, and held while the child runs;
  - at most 64 live children, 2 MiB of argv and env per spawn, 16 MiB of
    output per nested run and 64 KiB per pipe;
  - a cap of 16Ki resource handles per child store.
- **Filesystem.** A child sees a fork of the session's VFS: the same mounts
  with the same permissions and the same quota, over WASI 0.2 and 0.3 alike.
  Without a session filesystem it gets an empty in-memory one, never the host
  filesystem. The only mount added is the proxy's CA certificate, read-only.
- **Environment.** Children see no host environment variables, only what the
  script passes and the proxy settings, which they can't override.
- **Network.** Children have no sockets of their own beyond connections to
  their proxy: no DNS, no UDP, no listening. Connections are counted per
  child, per execution and per process. The proxy checks every request
  (each one on a kept-alive connection too) as strictly as `eryx:net`
  checks the guest's own, with the same parser and rules:
  - CONNECT targets are checked by host *and* resolved address, and only the
    checked addresses are dialled;
  - blind tunnels are refused for hosts with HTTP rules or credentials, and
    for commands not allowed to open them;
  - HTTP/2, and anything after an Upgrade, is refused.
- **Per-command networks.** A command can be given no network or a narrower
  policy than the sandbox's. Narrowing is an intersection: every check runs
  in every policy in the chain, and limits take the smallest value. A nested
  child gets only what both its parent's network and its own command's
  allow, so it's never wider than the child that spawned it.
- **TLS interception.** Each child proxy terminates TLS with a CA it
  generates itself. Its private key stays in host memory; children get only
  the certificate, mounted read-only, and trust nothing else. Proxies belong
  to one execution, and their CAs are dropped when it ends, so no CA is
  trusted across sandboxes or executions.
- **Secrets and credentials.** Children see only placeholders. The proxy
  substitutes a secret into request headers only, only for that secret's
  hosts, after the request has passed every check. Credentials the host adds
  for a command (`with_credential`) are chosen by connection host and
  transport, go only to that host, and never reach the child. Each
  credentialed command gets its own proxy with its own password. Secrets,
  credential values and proxy passwords are scrubbed from output, traces
  and errors.
- **Command files.** `Command::from_file_expecting` and `load_cached` read
  a command file once and check its sha256 against the pinned digest before
  compiling those same bytes. Compiled commands are cached under the private
  eryx cache directory, keyed by that digest, the eryx version and the
  engine's compatibility hash, never by path. gRPC clients can't name
  command files; `eryx-server` registers commands only from operator flags.

### The JavaScript guest's network

The JS guest's `fetch` uses the same `eryx:net` imports as Python, so the
same network policy and secret substitution apply; there's no host HTTP
client behind it. Node built-ins that would reach the host (`fs`,
`child_process`, `net`, `http`, `os` and so on) are stubs that throw.

## Components that matter most / least

Most important:

1. Anything that lets guest code affect the host process outside its
   granted capabilities. That includes memory corruption, panics/aborts, `unsafe`
   misuse, out-of-bounds reads of host memory, and escaping the VFS root or
   reaching the real filesystem.
2. Resource bounds: guest-controlled sizes, lengths, offsets or counts that make
   the host allocate, loop or block without limit despite configured limits
   (memory limit, fuel, execution timeout, VFS quota, callback limits).
3. Network policy enforcement in `net.rs` and the child proxy (`net/`):
   reaching hosts or ports the policy forbids, including via DNS, redirects,
   IP literal encodings, request framing or TLS, or a child reaching more
   than its command's network allows.
4. Secret handling: the real secret value, a proxy-added credential or the
   per-sandbox CA's private key reaching the guest or a child, or a secret,
   credential or proxy password appearing unscrubbed in stdout, stderr,
   results, errors, traces or snapshots.
5. Isolation between separate `execute()` calls on one `Sandbox`, and between
   separate sessions. One execution must not observe another's state.

Lower priority but in scope: `eryx-python` (PyO3 bindings) and the JS
bindings in `js/`, where they handle guest-produced data.

Out of scope:

- Code that only runs inside a guest or a child (`crates/eryx-guest-*/`,
  CPython, QuickJS, brush, command components) misbehaving *within* the
  sandbox. Guests and children are already assumed hostile, so a crash or a
  bug in them is not a vulnerability unless it changes host behaviour.
- Bugs in Wasmtime, CPython, wasi-libc or other upstream dependencies. Report
  those upstream. A bug in how *eryx* configures or uses them is in scope.
- `eryx-precompile`, `eryx-build`, `eryx-bundler`, environment packing,
  build scripts, CI, benchmarks and examples. These run on developer or
  operator machines with trusted input.
- Timing and cache side channels, and fairness between concurrent tenants.
- Reusing one live `Session` across mutually untrusted users. It deliberately
  keeps interpreter state between calls, and `clear_state()`/`reset()` are not
  documented as tenant isolation boundaries.

## How to exercise it

- `cargo nextest run --workspace --all-features` runs the full suite.
  Test binaries are prebuilt in the image.
- `crates/eryx/examples/` has small drivers, such as `simple.rs`,
  `runtime_callbacks.rs` and `resource_limits.rs`. Run them with
  `cargo run -p eryx --example <name> --features embedded`.
- The quickest reproducer shape is a Rust integration test in
  `crates/eryx/tests/` that builds a `Sandbox` with `Sandbox::embedded()` and
  calls `execute()` on a hostile Python script.
- For child processes, build `Sandbox::embedded().shell()` with
  `.with_command("tc", ...)` from the fixture in `tests/fixtures/test-cmd`, as
  `tests/child_network.rs` and `tests/commands.rs` do, and run them with
  `cargo nextest run -p eryx --features embedded,shell --test child_network`
  (or `--test commands`).
- `crates/eryx-server/tests/` has gRPC end-to-end tests.

## How you rate severity

- **Critical**: guest code achieves code execution in the host, reads or
  writes host memory, or reads or writes the host filesystem outside the
  configured VFS/mounts.
- **High**: guest code reliably crashes or aborts the host process (panic,
  abort, OOM) under a configuration that sets resource limits. Also high: the
  real value of a secret reaches the guest or unscrubbed output, network
  policy is bypassed to reach a forbidden host, or one execution reads another
  execution's or session's data.
- **Medium**: guest code makes the host consume unbounded memory, CPU or
  tasks past its configured limits without crashing it, or keeps host work
  running after the execution ends. Also medium: a limit (timeout, fuel, memory,
  quota) that does not apply on some code path.
- **Low**: problems that need an unusual or explicitly unsafe host
  configuration (for example, unlimited resources or all-hosts networking), or
  only produce confusing errors.

A host panic reachable from guest input is at least medium even without a
demonstrated crash of a real deployment, because embedders run eryx in-process.

## Anything to leave alone

- Do not report that the sandbox "executes arbitrary Python". That is the
  product.
- Do not report capabilities the host grants on purpose. A callback the host
  registered doing what it was written to do, or networking to a host on the
  allowlist, is expected behaviour. So is a child reaching what its
  command's network allows, which is no more than the script's own
  `eryx:net` access, or a command doing what it was built to do with the
  argv it's given.
- Children of one execution share that execution's budgets and filesystem;
  they aren't isolated from each other or from the script.
- The child proxy decrypting children's TLS with its per-sandbox CA is
  intended interception, not a MITM finding.
- The network byte budget (`max_bytes`) is a Rust API setting. gRPC requests
  are bounded by connection limits and timeouts.
- Patches should keep the public API stable where possible and come with a
  regression test.
