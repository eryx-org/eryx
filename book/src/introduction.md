# Introduction

**Eryx** is a WebAssembly sandbox for running untrusted code. It runs **Python** (real CPython 3.14), **JavaScript** (QuickJS) and a **bash-compatible shell** that runs real CLI tools. It's designed for AI agents and applications that need to execute code safely, with fine-grained control over what that code can access.

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live _in_ sand.
> Perfect for code running inside a sandbox.

## Try It Now

```bash
uvx --with pyeryx eryx -c 'import sys; print(f"Python {sys.version} in a sandbox!")'
uvx --with 'pyeryx[shell]' eryx -l shell -c 'echo "a shell in a sandbox" | wc -w'
```

Or try it in the browser at [demo.eryx.run](https://demo.eryx.run).

## Features

### Three guest languages, one API

- **Python** — The full CPython 3.14 interpreter compiled to WebAssembly, using techniques from [componentize-py](https://github.com/bytecodealliance/componentize-py). It's not a reimplementation or a subset: `json`, `sqlite3`, `pathlib`, `asyncio` and the rest of the standard library work, and you can add `.whl` and `.tar.gz` packages, including WASI builds of native extensions.
- **JavaScript** — QuickJS (quickjs-ng), with `fetch` and `node:fs`.
- **Shell** — A bash-compatible shell (brush), with pipelines, nested `sh` and `bash`, shebang scripts and a built-in `grep`.
- Callbacks, resource limits, sessions and snapshots work the same way in each. See [Guest Languages](./guide/sandboxes.md#guest-languages).

### Real commands in the shell

- **External commands** — WASI command components, registered by name. uutils coreutils is embedded. [eryx-recipes](https://github.com/eryx-org/eryx-recipes) publishes signed builds of ripgrep, jq, curl, git, gh, sqlite3 and more, and [Building Commands](./guide/building-commands.md) shows how to build your own.
- **Pipelines and nested commands** — Pipeline stages stream into each other, and commands can run commands of their own (git runs git; go runs its compiler).
- **Per-command access** — Each command can get its own network policy, mounts and environment. See [External Commands](./guide/sandboxes.md#external-commands).

### Secure by default

- **Secrets management** — Secrets are never exposed to sandboxed code. Placeholders are substituted transparently only in HTTP requests to authorized hosts, and scrubbed from all outputs.
- **Hybrid filesystem** — An isolated virtual filesystem by default, with opt-in host directory mounts (read-only or read-write) for controlled file access.
- **Resource limits** — Configurable execution timeouts, memory caps, CPU fuel and filesystem entry limits, to prevent runaway code.
- **Network policies** — Networking is disabled by default. Opt in with host allowlists and blocklists, HTTP rules and connection limits.

### Fast startup

- **Pre-compiled Wasm** — Sandbox creation in ~10–20ms with ahead-of-time compilation, instead of a ~650ms cold start.
- **Sandbox pooling** — A managed pool of warm sandbox instances for high-throughput scenarios.
- **Pre-initialized runtimes** — Bake third-party packages into the Python runtime to skip per-sandbox install work. The JavaScript runtime is pre-initialized too.

### Host integration

- **Async callbacks** — Expose host functions as `async` functions (`await get_time()` in Python and JavaScript), with parallel execution via `asyncio.gather()`. In the shell, callbacks are commands. See also [Callback Replay & Suspension](./guide/callback-replay.md).
- **Real-time output streaming** — Stream stdout and stderr as they're produced, instead of buffering until completion.
- **Session state** — Variables, functions and classes persist between executions, for REPL-style use.
- **State snapshots** — Capture and restore Python state with dill-based serialization.
- **Execution tracing** — Line-level progress reporting via `sys.settrace`.
- **Execution cancellation** — Cancel long-running executions via `ExecutionHandle`.

### Many ways to run it

- **Embedded** — First-class bindings for Rust, Python and JavaScript (the browser and Node.js).
- **CLI** — Run sandboxed code from the terminal with `python -m eryx` or `uvx --with pyeryx eryx`. Pick a language with `-l`.
- **[MCP Server](./guide/mcp-server.md)** — `eryx serve` gives AI assistants a sandboxed `run_python` tool.
- **[gRPC Server](./guide/grpc-server.md)** — `eryx-server` runs all three languages for remote, multi-tenant execution, and is published as a Docker image.

## Use Cases

Eryx is designed for scenarios where you need to:

- Give AI agents a safe place to run Python, JavaScript or shell commands
- Execute untrusted or user-provided code with controlled access to host resources
- Give an agent real CLI tools (ripgrep, jq, git, gh and others) without giving it your machine
- Provide a sandboxed scripting environment in your application
- Build REPL-style interfaces with persistent state
- Create plugin systems with Python or JavaScript as the extension language

## How It Works

Eryx uses [Wasmtime](https://wasmtime.dev/) to run each guest language's runtime, compiled to WebAssembly. Shell commands are WebAssembly components too, so everything a sandbox runs is isolated in the same way:

- **Memory isolation** — Sandboxed code cannot access host memory
- **File system isolation** — No access to host files by default; opt-in via VFS or volume mounts
- **Network isolation** — Networking is opt-in with configurable policies
- **Resource limits** — Configurable timeouts, memory limits, and callback restrictions

Host callbacks are implemented using the WebAssembly Component Model, allowing Rust, Python or JavaScript host code to expose async functions that sandboxed code can call.

## Next Steps

- [Installation](./getting-started/installation.md) - Get started with Eryx
- [Quick Start](./getting-started/quick-start.md) - Your first sandbox
- [Core Concepts](./getting-started/core-concepts.md) - Understand the architecture
