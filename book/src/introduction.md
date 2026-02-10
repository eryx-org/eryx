# Introduction

**Eryx** is a library for running Python code in a WebAssembly sandbox. It's designed for AI agents and applications that need to execute untrusted Python safely, with fine-grained control over what the code can access.

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live _in_ sand.
> Perfect for "Python running inside a sandbox."

## Try It Now

```bash
uvx --with pyeryx eryx -c 'import sys; print(f"Python {sys.version} in a sandbox!")'
```

Or try it in the browser at [demo.eryx.run](https://demo.eryx.run).

## Features

### Secure by default

- **Secrets management** — Secrets are never exposed to sandboxed code; placeholders are substituted transparently only in HTTP requests to authorized hosts, and scrubbed from all outputs
- **Hybrid filesystem** — Isolated virtual filesystem by default, with opt-in host directory mounts (read-only or read-write) for controlled file access
- **Resource limits** — Configurable execution timeouts, memory caps, and CPU fuel limits to prevent runaway code
- **Network policies** — Networking is disabled by default; opt-in with host allowlists, blocklists, and connection limits

### Fast startup

- **Pre-compiled Wasm** — Sandbox creation in ~10–20ms with ahead-of-time compilation (vs ~650ms cold start)
- **Sandbox pooling** — Managed pool of warm sandbox instances for high-throughput scenarios
- **Pre-initialized packages** — Bake third-party packages into the Wasm runtime to eliminate per-sandbox install overhead

### Host integration

- **Async callbacks** — Expose host functions as `async` Python functions (e.g., `await get_time()`) with parallel execution via `asyncio.gather()`
- **Real-time output streaming** — Stream stdout/stderr as it's produced, instead of buffering until completion
- **Session state** — Variables, functions, and classes persist between executions for REPL-style usage
- **State snapshots** — Capture and restore Python state with dill-based serialization
- **Execution tracing** — Line-level progress reporting via `sys.settrace`
- **Execution cancellation** — Cancel long-running executions via `ExecutionHandle`

### True CPython

- **Real CPython 3.14** — Not a reimplementation or subset; runs the full CPython interpreter compiled to WebAssembly using techniques from [componentize-py](https://github.com/bytecodealliance/componentize-py)
- **Install packages** — Add `.whl` and `.tar.gz` packages including WASI-compiled native extensions
- **Full standard library** — `json`, `sqlite3`, `pathlib`, `re`, `asyncio`, and the rest of the stdlib work out of the box

### Multi-language support

- **Rust, Python, and JavaScript** — First-class bindings for all three
- **CLI** — Run sandboxed Python from the terminal with `python -m eryx` or `uvx --with pyeryx eryx`

## Use Cases

Eryx is designed for scenarios where you need to:

- Give AI agents a safe Python execution environment
- Execute untrusted or user-provided code with controlled access to host resources
- Provide a sandboxed scripting environment in your application
- Build REPL-style interfaces with persistent state
- Create plugin systems with Python as the extension language

## How It Works

Eryx uses [Wasmtime](https://wasmtime.dev/) to run CPython compiled to WebAssembly. This provides strong isolation guarantees:

- **Memory isolation** — Sandboxed code cannot access host memory
- **File system isolation** — No access to host files by default; opt-in via VFS or volume mounts
- **Network isolation** — Networking is opt-in with configurable policies
- **Resource limits** — Configurable timeouts, memory limits, and callback restrictions

Host callbacks are implemented using the WebAssembly Component Model, allowing Rust, Python, or JavaScript host code to expose async functions that sandboxed Python can call.

## Next Steps

- [Installation](./getting-started/installation.md) - Get started with Eryx
- [Quick Start](./getting-started/quick-start.md) - Your first sandbox
- [Core Concepts](./getting-started/core-concepts.md) - Understand the architecture
