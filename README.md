# Eryx

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live *in* sand.
> Perfect for "Python running inside a sandbox."

A Rust library that executes Python code in a WebAssembly sandbox with async callbacks.

## Features

- **Async callback mechanism** — Callbacks are exposed as direct async functions (e.g., `await get_time()`)
- **Parallel execution** — Multiple callbacks can run concurrently via `asyncio.gather()`
- **Session state persistence** — Variables, functions, and classes persist between executions for REPL-style usage
- **State snapshots** — Capture and restore Python state with pickle-based serialization
- **Execution tracing** — Line-level progress reporting via `sys.settrace`
- **Introspection** — Python can discover available callbacks at runtime
- **Composable runtime libraries** — Pre-built APIs with Python wrappers and type stubs
- **Pre-compiled Wasm** — 41x faster sandbox creation with ahead-of-time compilation

## Python Version

Eryx embeds **CPython 3.14** compiled to WebAssembly (WASI). The WASI-compiled CPython and standard library come from the [componentize-py](https://github.com/bytecodealliance/componentize-py) project by the Bytecode Alliance.

> **Note:** Python 3.14 is currently in development. Eryx tracks componentize-py's CPython builds, which follow CPython's main branch.

## Quick Start

```rust
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Sandbox::embedded() provides zero-config setup (requires `embedded` feature)
    let sandbox = Sandbox::embedded().build()?;

    let result = sandbox.execute(r#"
        print("Hello from Python!")
    "#).await?;

    println!("Output: {}", result.stdout);
    Ok(())
}
```

## With Callbacks (TypedCallback)

Use `TypedCallback` for strongly-typed callbacks with automatic schema generation:

```rust
use std::{future::Future, pin::Pin};

use eryx::{TypedCallback, CallbackError, Sandbox, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

// Arguments struct - schema is auto-generated from this
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// The message to echo back
    message: String,
}

struct Echo;

impl TypedCallback for Echo {
    type Args = EchoArgs;

    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echoes back the message" }

    fn invoke_typed(&self, args: EchoArgs) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            Ok(json!({ "echoed": args.message }))
        })
    }
}

// For no-argument callbacks, use `()` as the Args type
struct GetTime;

impl TypedCallback for GetTime {
    type Args = ();

    fn name(&self) -> &str { "get_time" }
    fn description(&self) -> &str { "Returns the current Unix timestamp" }

    fn invoke_typed(&self, _args: ()) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Ok(json!(now))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_callback(GetTime)
        .with_callback(Echo)
        .build()?;

    let result = sandbox.execute(r#"
# Callbacks are available as direct async functions
timestamp = await get_time()
print(f"Current time: {timestamp}")

response = await echo(message="Hello!")
print(f"Echo: {response}")
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

For runtime-defined callbacks (plugin systems, dynamic APIs), implement the `Callback` trait directly.
See the `runtime_callbacks` example.

## Session State Persistence

For REPL-style usage where state persists between executions:

```rust
use eryx::{Sandbox, session::InProcessSession};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // First execution defines a variable
    session.execute("x = 42").await?;

    // Second execution can access it
    let result = session.execute("print(x * 2)").await?;
    println!("{}", result.stdout); // "84"

    // Snapshot and restore state
    let snapshot = session.snapshot_state().await?;
    session.clear_state().await?;
    session.restore_state(&snapshot).await?;

    Ok(())
}
```

## Feature Flags

| Feature              | Description                                                                         | Trade-offs                               |
|----------------------|-------------------------------------------------------------------------------------|------------------------------------------|
| `embedded`           | Zero-config sandboxes: embeds pre-compiled Wasm runtime + Python stdlib             | +32MB binary size; enables `unsafe` code paths |
| `preinit`            | Pre-initialization support for ~25x faster sandbox creation                         | Adds `eryx-runtime` dep; requires build step |
| `native-extensions`  | Native Python extension support (e.g., numpy) via late-linking                      | Implies `preinit`; experimental          |

Package support (`with_package()` for `.whl` and `.tar.gz` files) is always available — no feature flag required.

### Pre-initialization

The `preinit` feature provides ~25x faster sandbox creation by capturing Python's initialized memory state at build time. This works with or without native extensions — you can pre-import stdlib modules like `json`, `asyncio`, `re`, etc.

| Metric | Without Pre-init | With Pre-init | Speedup |
|--------|-----------------|---------------|---------|
| Sandbox creation | ~450ms | ~18ms | **25x faster** |

### Recommended Configurations

```rust
// Fastest startup, zero configuration (recommended for most users)
// Features: embedded
let sandbox = Sandbox::embedded().build()?;

// With pre-initialization for faster sandbox creation
// Features: embedded, preinit
// Pre-import common stdlib modules during build for ~25x speedup
let preinit_bytes = eryx::preinit::pre_initialize(
    &stdlib_path, None, &["json", "asyncio", "re"], &[]
).await?;

// With package support for third-party libraries
// Features: embedded (packages always available)
let sandbox = Sandbox::embedded()
    .with_package("requests-2.31.0-py3-none-any.whl")?
    .build()?;

// With native extensions (numpy, etc.)
// Features: embedded, native-extensions
let sandbox = Sandbox::embedded()
    .with_package("numpy-wasi.tar.gz")?
    .build()?;
```

## Performance

| Metric | Normal Wasm | Pre-compiled | Speedup |
|--------|-------------|--------------|---------|
| Sandbox creation | ~650ms | ~16ms | **41x faster** |
| Per-execution overhead | ~1.8ms | ~1.6ms | 14% faster |
| Session (5 executions) | ~70ms | ~3ms | **23x faster** |

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
mise run build-eryx-runtime  # Build the Python Wasm component
mise run build-all      # Build Wasm + Rust crates
mise run precompile-eryx-runtime # Pre-compile to native code

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
# Clean all caches and rebuild
mise run clean-artifacts
cargo clean
rm -rf /tmp/eryx-embedded
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

## Project Structure

```
eryx/
├── Cargo.toml              # Workspace root
├── Cargo.lock              # Dependency lock file
├── mise.toml               # mise configuration and tasks
├── rustfmt.toml            # Formatting configuration
├── .config/
│   └── nextest.toml        # nextest configuration
├── .github/
│   └── workflows/          # CI workflows
├── crates/
│   ├── eryx/               # Core library crate
│   │   ├── Cargo.toml
│   │   ├── build.rs        # Pre-compilation for embedded-runtime
│   │   ├── benches/        # Criterion benchmarks
│   │   ├── examples/       # Example programs
│   │   ├── tests/          # Integration tests
│   │   └── src/
│   │       ├── lib.rs      # Public API exports
│   │       ├── sandbox.rs  # Sandbox struct, execute()
│   │       ├── callback.rs # Callback trait, CallbackError
│   │       ├── library.rs  # RuntimeLibrary struct
│   │       ├── trace.rs    # TraceEvent, TraceHandler
│   │       ├── wasm.rs     # wasmtime setup, PythonExecutor
│   │       ├── error.rs    # Error types
│   │       └── session/    # Session state persistence
│   │           ├── mod.rs
│   │           ├── executor.rs   # SessionExecutor
│   │           └── in_process.rs # InProcessSession
│   ├── eryx-runtime/       # Python Wasm runtime packaging
│   │   ├── Cargo.toml
│   │   ├── build.rs        # Links eryx-wasm-runtime + libpython + WASI libs
│   │   ├── runtime.wit     # WIT interface definition
│   │   ├── runtime.wasm    # Built Wasm component (~47MB)
│   │   ├── runtime.cwasm   # Pre-compiled native code (~52MB)
│   │   └── libs/           # WASI libraries (zstd compressed)
│   └── eryx-wasm-runtime/  # Rust runtime implementation (compiled to Wasm)
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs      # WIT export implementations
│           └── python.rs   # Python interpreter FFI, tracing
└── docs/plans/             # Design documents
```

## Inspiration & Acknowledgements

Eryx is heavily inspired by and closely related to [**componentize-py**](https://github.com/bytecodealliance/componentize-py/), a Bytecode Alliance project that pioneered running Python in WebAssembly via the Component Model. Eryx builds on the same foundational work (CPython compiled to Wasm, WASI support) but takes a different architectural approach. Python bindings are also available, allowing you to run sandboxed Python from within a Python host.

This project builds on excellent work from the [Bytecode Alliance](https://bytecodealliance.org/):

- [**wasmtime**](https://github.com/bytecodealliance/wasmtime) — The WebAssembly runtime that powers eryx's sandboxed execution
- [**wasm-tools**](https://github.com/bytecodealliance/wasm-tools) — WebAssembly tooling including `wit-component`, `wit-parser`, and component linking
- [**componentize-py**](https://github.com/bytecodealliance/componentize-py) — The foundation for running CPython in Wasm, including the WASI-compatible Python build
- [**component-init**](https://github.com/dicej/component-init) — Pre-initialization support for faster sandbox startup (by [@dicej](https://github.com/dicej))

### Comparison with componentize-py

| Aspect                     | componentize-py                                      | eryx                                                 |
|----------------------------|------------------------------------------------------|------------------------------------------------------|
| **Primary Use Case**       | Build Python *components* that export WIT interfaces | Embed Python as a *sandbox* within a Rust host       |
| **Direction of Control**   | Python exports functions for hosts to call           | Rust host executes Python code and exposes callbacks |
| **WIT Usage**              | Python implements WIT worlds (exports)               | Internal implementation detail (not user-facing)     |
| **Output**                 | Standalone `.wasm` component files                   | In-process sandboxed execution                       |
| **Async Model**            | Component Model async (if supported)                 | Python `asyncio` with Rust `async` callbacks         |
| **Target Audience**        | Python developers building Wasm components           | Rust/Python developers embedding sandboxed scripting |
| **State Management**       | Stateless component invocations                      | Session persistence, snapshots, REPL-style           |
| **Package Loading**        | Build-time only (bundled into component)             | Dynamic at runtime via `with_package()`              |

**When to use componentize-py:**
- You're building a Python application to distribute as a Wasm component
- You want Python to implement a WIT interface that other components/hosts consume
- You're working in a component-model-native ecosystem (e.g., wasmCloud, Spin)

**When to use eryx:**
- You're building a Rust or Python application that needs to run user-provided Python code
- You need a sandboxed scripting environment with controlled host callbacks
- You want REPL-style sessions with state persistence between executions
- You need fine-grained execution tracing and resource limits

## License

MIT OR Apache-2.0
