# Threat model

## What this project does and where untrusted input enters

Eryx runs untrusted Python inside a WebAssembly sandbox. CPython 3.14 is
compiled to a WASM component and executed with Wasmtime. The host (the
application embedding eryx) gives the guest capabilities explicitly:
async callbacks, a virtual filesystem, policy-controlled TCP/TLS, and secrets
that the guest only ever sees as placeholders.

**Guest Python code is hostile.** Assume an attacker controls the full text of
every script passed to `execute()`, and anything it computes at runtime:
callback arguments, stdout/stderr bytes, file contents and offsets written to
the VFS, hostnames and ports it connects to, bytes it sends, and the values it
returns. The attacker cannot change the host's configuration.

Untrusted input reaches the host through:

- **The guest↔host boundary.** These are the WIT imports in
  `crates/eryx-runtime/wit/` and their host implementations:
  callbacks (`crates/eryx/src/callback*.rs`), networking
  (`crates/eryx/src/net.rs`), the VFS (`crates/eryx-vfs/`), output and trace
  reporting (`crates/eryx/src/trace.rs`, `wasm.rs`), and secret substitution
  (`crates/eryx/src/secrets.rs`).
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
wheels it chooses to load, the precompiled `runtime.cwasm` and everything in
`crates/eryx-runtime/libs/` and `prebuilt/`.

## Components that matter most / least

Most important:

1. Anything that lets guest code affect the host process outside its
   granted capabilities. That includes memory corruption, panics/aborts, `unsafe`
   misuse, out-of-bounds reads of host memory, and escaping the VFS root or
   reaching the real filesystem.
2. Resource bounds: guest-controlled sizes, lengths, offsets or counts that make
   the host allocate, loop or block without limit despite configured limits
   (memory limit, fuel, execution timeout, VFS quota, callback limits).
3. Network policy enforcement in `net.rs`: reaching hosts or ports the policy
   forbids, including via DNS, redirects, IP literal encodings or TLS.
4. Secret handling: the real secret value reaching the guest, or appearing
   unscrubbed in stdout, stderr, results, errors, traces or snapshots.
5. Isolation between separate `execute()` calls on one `Sandbox`, and between
   separate sessions. One execution must not observe another's state.

Lower priority but in scope: `eryx-python` (PyO3 bindings) and the JS
bindings in `js/`, where they handle guest-produced data.

Out of scope:

- Code that only runs inside the guest (`crates/eryx-wasm-runtime/`, CPython)
  misbehaving *within* the sandbox. The guest is already assumed hostile, so a
  guest crash or a guest Python bug is not a vulnerability unless it changes
  host behaviour.
- Bugs in Wasmtime, CPython, wasi-libc or other upstream dependencies. Report
  those upstream. A bug in how *eryx* configures or uses them is in scope.
- `eryx-precompile`, build scripts, CI, benchmarks and examples. These run on
  developer machines with trusted input.
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
  allowlist, is expected behaviour.
- Patches should keep the public API stable where possible and come with a
  regression test.
