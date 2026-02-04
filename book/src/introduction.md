# Introduction

**Eryx** is a library that executes Python code in a WebAssembly sandbox with async callbacks.

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live *in* sand.
> Perfect for "Python running inside a sandbox."

## Features

- **Async callback mechanism** — Callbacks are exposed as direct async functions (e.g., `await get_time()`)
- **Parallel execution** — Multiple callbacks can run concurrently via `asyncio.gather()`
- **Session state persistence** — Variables, functions, and classes persist between executions for REPL-style usage
- **State snapshots** — Capture and restore Python state with pickle-based serialization
- **Execution tracing** — Line-level progress reporting via `sys.settrace`
- **Stderr capture** — Separate stdout and stderr streams with optional streaming handlers
- **Execution cancellation** — Cancel long-running executions via `ExecutionHandle`
- **TCP/TLS networking** — Host-controlled network access with configurable policies
- **Introspection** — Python can discover available callbacks at runtime
- **Composable runtime libraries** — Pre-built APIs with Python wrappers and type stubs
- **Pre-compiled Wasm** — 41x faster sandbox creation with ahead-of-time compilation
- **Sandbox pooling** — Managed pool of warm sandbox instances for high-throughput scenarios

## Python Version

Eryx embeds **CPython 3.14** compiled to WebAssembly (WASI). The WASI-compiled CPython and standard library come from the [componentize-py](https://github.com/bytecodealliance/componentize-py) project by the Bytecode Alliance.

## Use Cases

Eryx is designed for scenarios where you need to:

- Execute untrusted Python code safely
- Provide a sandboxed scripting environment in your application
- Run user-provided code with controlled access to host resources
- Build REPL-style interfaces with persistent state
- Create plugin systems with Python as the extension language

## How It Works

Eryx uses [Wasmtime](https://wasmtime.dev/) to run CPython compiled to WebAssembly. This provides strong isolation guarantees:

- **Memory isolation** — Sandboxed code cannot access host memory
- **File system isolation** — No access to host files by default
- **Network isolation** — Networking is opt-in with configurable policies
- **Resource limits** — Configurable timeouts, memory limits, and callback restrictions

Host callbacks are implemented using the WebAssembly Component Model, allowing Rust or Python host code to expose async functions that sandboxed Python can call.

## Next Steps

- [Installation](./getting-started/installation.md) - Get started with Eryx
- [Quick Start](./getting-started/quick-start.md) - Your first sandbox
- [Core Concepts](./getting-started/core-concepts.md) - Understand the architecture
