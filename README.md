# Eryx

[![CI](https://github.com/eryx-org/eryx/actions/workflows/ci.yml/badge.svg)](https://github.com/eryx-org/eryx/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/eryx.svg)](https://crates.io/crates/eryx)
[![PyPI](https://img.shields.io/pypi/v/pyeryx.svg)](https://pypi.org/project/pyeryx/)
[![npm](https://img.shields.io/npm/v/@eryx-org/eryx.svg)](https://www.npmjs.com/package/@eryx-org/eryx)
[![docs](https://img.shields.io/badge/docs-docs.eryx.run-blue)](https://docs.eryx.run)

A WebAssembly sandbox for running untrusted code from AI agents and users: **Python** (real CPython 3.14), **JavaScript** (QuickJS), and a **bash-compatible shell** that runs real CLI tools such as coreutils, ripgrep, jq, curl, git and gh. It has memory and CPU limits, and no filesystem or network access by default. Networking and secrets are host-controlled, and async host callbacks give the access you _do_ want to allow.

Embed it from **Rust**, **Python** or **JavaScript**, or run it as an **MCP server** or a **gRPC server**. [Try the demo](https://demo.eryx.run) or [read the docs](https://docs.eryx.run).

> **Used in production at [Grafana](https://grafana.com/).**

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live _in_ sand.
> Perfect for code running inside a sandbox.

## Features

### Three guest languages, one API

- **Python**: full CPython 3.14, not a subset, so the standard library and most pure-Python packages just work. Add `.whl` and `.tar.gz` packages, including WASI builds of native extensions.
- **JavaScript**: QuickJS (quickjs-ng), with `fetch` and `node:fs`.
- **Shell**: a bash-compatible shell (brush). Pipelines, nested `sh` and `bash`, and shebang scripts work, and `grep` is built in.
- Callbacks, resource limits, sessions and snapshots work the same way in each.

### Real commands in the shell

- External commands are WASI command components: uutils coreutils is embedded, and you register others by name.
- [eryx-recipes](https://github.com/eryx-org/eryx-recipes) publishes signed builds of ripgrep, jq, curl, git, gh, sqlite3 and more to `ghcr.io/eryx-org/commands`. To build your own, see [Building Commands](https://docs.eryx.run/latest/book/guide/building-commands.html).
- A pipeline's stages stream into each other, and commands can run commands of their own (git runs git; go runs its compiler).
- Each command can get its own network policy, mounts and environment.

### Secure by default

- **Filesystem**: an isolated virtual filesystem, with opt-in host directory mounts, read-only or writable.
- **Resource limits**: execution timeouts, memory caps, CPU fuel and filesystem entry limits.
- **Networking**: off by default. Opt in with host allowlists and blocklists, HTTP rules and connection limits.
- **Secrets**: sandboxed code only ever sees placeholders. The real values are substituted only in requests to the hosts you allow, and scrubbed from output.

### Host integration

- **Async callbacks**: host functions called as `await get_time()` in Python and JavaScript, or as commands in the shell. They can run in parallel, and can be replayed and suspended.
- **Sessions and snapshots**: state persists between executions, for REPL-style use, and can be saved and restored.
- **Output streaming, tracing and cancellation**.

### Ways to run it

- **Embedded**, from Rust, Python or JavaScript (the browser or Node.js).
- **A CLI**: `uvx --with pyeryx eryx -c 'print("hi")'`, with `-l javascript` or `-l shell` for the other languages.
- **An MCP server** (`eryx serve`, with `pyeryx[serve]`) that gives AI assistants a `run_python` tool.
- **[eryx-server](https://docs.eryx.run/latest/book/guide/grpc-server.html)**, a gRPC server for remote, multi-tenant execution in all three languages, published as a Docker image.
- **Fast**: pre-compiled and pre-initialized runtimes, and a pool of warm instances. See [Performance Tuning](https://docs.eryx.run/latest/book/guide/performance.html).

## Quick Start

### Python

```bash
pip install pyeryx                # or pyeryx[javascript], pyeryx[shell], pyeryx[all]
```

> The package is published as `pyeryx` but imported as `eryx`.

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute('print(f"2 + 2 = {2 + 2}")')
print(result.stdout_text)

# With pyeryx[shell]:
sh = eryx.Sandbox(language="shell")
print(sh.execute("printf 'b\\na\\nc\\n' | sort | head -1").stdout_text)
```

See [`crates/eryx-python`](crates/eryx-python/README.md) for the full Python API.

### JavaScript

```bash
npm install @eryx-org/eryx
```

```js
import { Sandbox } from "@eryx-org/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute('print("Hello from Python!")');
console.log(result.stdout);

const sh = new Sandbox({ language: "shell", callbacks: { add: ({ a, b }) => a + b } });
await sh.execute('echo "sum $(add --a 2 --b 3)"'); // stdout "sum 5\n"
```

See [`js/eryx`](js/eryx/README.md) for browser and Node.js requirements.

### Rust

```bash
cargo add eryx --features embedded,macros   # add `javascript` and `shell` for those languages
```

```rust
use eryx::{callback, CallbackError, Sandbox};
use serde_json::{json, Value};

/// Echoes back the message
#[callback]
async fn echo(message: String) -> Result<Value, CallbackError> {
    Ok(json!({ "echoed": message }))
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().with_callback(echo).build()?;

    let result = sandbox
        .execute(r#"print(await echo(message="Hello!"))"#)
        .await?;
    println!("{}", result.stdout_text());
    Ok(())
}
```

For callbacks defined at run time (plugin systems, dynamic APIs), implement the `Callback` trait directly; see the `runtime_callbacks` example.

## Learn more

The [docs](https://docs.eryx.run) cover everything else:

- [Sandboxes](https://docs.eryx.run/latest/book/guide/sandboxes.html): guest languages, the shell and its external commands
- [Callbacks](https://docs.eryx.run/latest/book/guide/callbacks.html), and [replay and suspension](https://docs.eryx.run/latest/book/guide/callback-replay.html)
- [Sessions](https://docs.eryx.run/latest/book/guide/sessions.html) and the [sandbox pool](https://docs.eryx.run/latest/book/guide/sandbox-pool.html)
- [VFS and file persistence](https://docs.eryx.run/latest/book/guide/vfs.html), [resource limits](https://docs.eryx.run/latest/book/guide/resource-limits.html), [networking](https://docs.eryx.run/latest/book/guide/networking.html) and [secrets](https://docs.eryx.run/latest/book/guide/secrets.html)
- [Packages](https://docs.eryx.run/latest/book/guide/packages.html) and [environments](https://docs.eryx.run/latest/book/guide/environments.html)
- [The MCP server](https://docs.eryx.run/latest/book/guide/mcp-server.html) and [the gRPC server](https://docs.eryx.run/latest/book/guide/grpc-server.html)
- [Pre-compiling runtimes](https://docs.eryx.run/latest/book/guide/precompile.html) and [performance tuning](https://docs.eryx.run/latest/book/guide/performance.html)
- [Installation](https://docs.eryx.run/latest/book/getting-started/installation.html), including the Rust feature flags

## Development

This project uses [mise](https://mise.jdx.dev/) for tooling and task management.

### Setup

```bash
mise install
mise run setup  # Build Wasm + precompile (one-time)
```

### Tasks

```bash
# Development
mise run check          # Run cargo check
mise run build          # Build all crates
mise run test           # Run tests with embedded Wasm
mise run test-all       # Run tests with all features
mise run lint           # Run clippy lints
mise run fmt            # Format code
mise run fmt-check      # Check code formatting

# Wasm
mise run build-runtime-python  # Build the Python Wasm component
mise run build-all      # Build Wasm + Rust crates
mise run precompile-runtime-python-no-preinit # Pre-compile to native code

# CI & Quality
mise run ci             # Run all CI checks (fmt-check, lint, test)
mise run msrv           # Check compilation on minimum supported Rust version

# Documentation
mise run doc            # Generate documentation
mise run doc-open       # Generate and open documentation

# Benchmarks
mise run bench          # Run benchmarks
mise run bench-save     # Run benchmarks and save baseline

# Examples
mise run examples       # Run all examples
```

### Manual Commands

```bash
cargo nextest run --workspace                    # Run tests
cargo nextest run --workspace --features embedded     # Fast tests
cargo clippy --workspace --all-targets --all-features # Run lints
cargo fmt --all                                  # Format code
cargo doc --workspace --no-deps --open           # Generate docs
cargo bench --package eryx                       # Run benchmarks
```

### Troubleshooting: Stale Builds

This project has multiple cache layers (cargo, mise, embedded runtime, WASM artifacts). If you experience unexpected behavior after changing code:

```bash
# Diagnose which cache layer is stale
mise run check-caches

# Clean all caches and rebuild
mise run clean-artifacts
cargo clean
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/eryx" "${TMPDIR:-/tmp}/eryx-$(id -u)"
mise run setup

# For Python binding work specifically
cd crates/eryx-python && maturin develop --release
```

Common symptoms of stale caches:

- Code changes don't seem to take effect
- `SandboxFactory` behaves differently than `Sandbox`
- Tests pass locally but fail in CI (or vice versa)

See `AGENTS.md` for detailed documentation on cache layers.

## Examples

All examples require the `embedded` feature:

```bash
cargo run --example simple --features embedded              # Basic usage with TypedCallback
cargo run --example runtime_callbacks --features embedded   # Runtime-defined callbacks (DynamicCallback)
cargo run --example with_tracing --features embedded        # Execution tracing and output handling
cargo run --example error_handling --features embedded      # Error handling scenarios
cargo run --example parallel_callbacks --features embedded  # Parallel execution verification
cargo run --example custom_library --features embedded      # Using RuntimeLibrary
cargo run --example session_reuse --features embedded       # Session state persistence
cargo run --example resource_limits --features embedded     # ResourceLimits usage
cargo run --example precompile --features embedded          # Pre-compilation demo
cargo run --example embedded_runtime --features embedded    # Embedded runtime
```

## Inspiration & Acknowledgements

Eryx is heavily inspired by and closely related to [**componentize-py**](https://github.com/bytecodealliance/componentize-py/), a Bytecode Alliance project that pioneered running Python in WebAssembly via the Component Model. Eryx's Python runtime builds on the same foundational work: the WASI-compiled CPython and standard library come from componentize-py. The difference is direction: componentize-py builds Python _components_ that export WIT interfaces for hosts to call, while eryx embeds a sandbox in your application, which runs code you hand it and calls back into your host.

This project builds on excellent work from the [Bytecode Alliance](https://bytecodealliance.org/) and others:

- [**wasmtime**](https://github.com/bytecodealliance/wasmtime) — The WebAssembly runtime that powers eryx's sandboxed execution
- [**wasm-tools**](https://github.com/bytecodealliance/wasm-tools) — WebAssembly tooling including `wit-component`, `wit-parser`, and component linking
- [**componentize-py**](https://github.com/bytecodealliance/componentize-py) — The foundation for running CPython in Wasm, including the WASI-compatible Python build
- [**component-init**](https://github.com/dicej/component-init) — Pre-initialization support for faster sandbox startup (by [@dicej](https://github.com/dicej))
- [**quickjs-ng**](https://github.com/quickjs-ng/quickjs), through [rquickjs](https://github.com/DelSkayn/rquickjs) — The JavaScript engine
- [**brush**](https://github.com/reubeno/brush) — The bash-compatible shell
- [**uutils coreutils**](https://github.com/uutils/coreutils) — The shell's default commands

## License

MIT OR Apache-2.0
