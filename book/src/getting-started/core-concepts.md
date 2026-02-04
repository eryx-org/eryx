# Core Concepts

This page explains the key concepts in Eryx and how they work together.

## Sandbox

A **Sandbox** is the main entry point for executing Python code in isolation. Each sandbox:

- Contains a WebAssembly instance of CPython 3.14
- Has isolated memory and no access to host resources by default
- Can be configured with callbacks, resource limits, and network access
- Is designed for one-off executions where state doesn't persist

Use sandboxes when each execution should start with a clean slate.

## Sessions

A **Session** wraps a sandbox and maintains Python state across multiple executions:

- Variables, functions, and classes persist between executions
- Useful for REPL-style interfaces and incremental computation
- Can optionally include a virtual filesystem for file persistence
- More efficient than creating new sandboxes for each execution

Use sessions when you need stateful, interactive Python execution.

## Callbacks

**Callbacks** are functions exposed from the host that sandboxed Python can call:

- Provide controlled access to host capabilities (database, API calls, etc.)
- Are async from both host and sandbox perspectives
- Can be strongly typed (Rust) or dynamically defined (Python)
- Allow sandboxed code to request information without direct access

Callbacks are the primary way sandboxed code interacts with the outside world.

## Resource Limits

**Resource Limits** control what sandboxed code can do:

- **Execution timeout** — Maximum time for code to run
- **Memory limits** — Maximum memory usage
- **Callback limits** — Maximum number of callback invocations
- **Callback timeout** — Maximum time for a single callback

Resource limits prevent runaway code from consuming excessive resources.

## Networking

**Networking** is disabled by default but can be enabled with policies:

- **Host filtering** — Allow/block specific domains with wildcard patterns
- **Connection limits** — Maximum concurrent connections
- **Timeouts** — Connection and I/O timeouts
- **TLS support** — Full HTTPS support with custom certificates

Network access is controlled by the host, not the sandboxed code.

## WebAssembly & WASI

Eryx uses WebAssembly (Wasm) and WASI for sandboxing:

- **WebAssembly** provides memory isolation and controlled execution
- **WASI** (WebAssembly System Interface) provides limited system access
- **Wasmtime** is the runtime that executes the Wasm code
- **Component Model** enables the callback mechanism

You don't need to understand these details to use Eryx, but they provide strong security guarantees.

## Pre-compilation

The `embedded` feature includes pre-compiled Wasm for fast startup:

- Python interpreter initialization (~450ms) is done at build time
- Sandbox creation is reduced from ~650ms to ~16ms (41x faster)
- The pre-compiled binary is embedded in your application
- Trade-off: adds ~32MB to your binary size

## Packages

Eryx supports loading Python packages at runtime:

- `.whl` files (standard Python wheels)
- `.tar.gz` / `.tgz` files (used by wasi-wheels project)
- Native extensions compiled for WASI
- Packages are loaded per-sandbox or via `SandboxFactory` for reuse

## Architecture Overview

```
┌─────────────────────────────────────────────┐
│          Your Application (Host)            │
│  ┌─────────────────────────────────────┐   │
│  │         Eryx Sandbox                │   │
│  │  ┌───────────────────────────────┐  │   │
│  │  │   WebAssembly Instance        │  │   │
│  │  │  ┌─────────────────────────┐  │  │   │
│  │  │  │   CPython 3.14          │  │  │   │
│  │  │  │   (Sandboxed Code)      │  │  │   │
│  │  │  └─────────────────────────┘  │  │   │
│  │  │            ↑                   │  │   │
│  │  │            │ Callbacks         │  │   │
│  │  │            ↓                   │  │   │
│  │  └───────────────────────────────┘  │   │
│  │         (Wasmtime Runtime)          │   │
│  └─────────────────────────────────────┘   │
└─────────────────────────────────────────────┘
```

## Sandbox vs Session Comparison

| Feature | Sandbox | Session |
|---------|---------|---------|
| **State persistence** | No — fresh each `execute()` | Yes — variables persist |
| **Virtual filesystem** | No | Optional |
| **Use case** | One-off execution | REPL, multi-step workflows |
| **Isolation** | Complete per-call | Complete from host |
| **Performance** | Moderate overhead | Lower overhead after warmup |

## Security Model

Eryx provides defense-in-depth security:

1. **Process-level isolation** — Wasm provides memory isolation
2. **Capability-based security** — Only explicitly granted capabilities are available
3. **Resource limits** — Prevent resource exhaustion
4. **Network policies** — Control which hosts can be accessed
5. **No filesystem access** — Unless explicitly enabled with VFS

This makes Eryx suitable for running untrusted Python code.

## Next Steps

- [Sandboxes Guide](../guide/sandboxes.md) - Detailed sandbox configuration
- [Callbacks Guide](../guide/callbacks.md) - Implementing callbacks
- [Sessions Guide](../guide/sessions.md) - Working with sessions
- [Resource Limits](../guide/resource-limits.md) - Configuring limits
