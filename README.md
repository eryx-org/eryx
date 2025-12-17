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
- **Pre-compiled WASM** — 41x faster sandbox creation with ahead-of-time compilation

## Quick Start

```rust
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::builder().build()?;

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
    let sandbox = Sandbox::builder()
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
    let sandbox = Sandbox::builder().build()?;
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
| `embedded`           | Zero-config sandboxes: embeds pre-compiled WASM runtime + Python stdlib             | +32MB binary size; enables `unsafe` code paths |
| `native-extensions`  | Native Python extension support (e.g., numpy) via late-linking and pre-initialization | Adds `eryx-runtime` dep; experimental    |

Package support (`with_package()` for `.whl` and `.tar.gz` files) is always available — no feature flag required.

### Recommended Configurations

```rust
// Fastest startup, zero configuration (recommended for most users)
// Features: embedded
let sandbox = Sandbox::builder().build()?;

// With package support for third-party libraries
// Features: embedded (packages always available)
let sandbox = Sandbox::builder()
    .with_package("requests-2.31.0-py3-none-any.whl")?
    .build()?;

// With native extensions (numpy, etc.)
// Features: embedded, native-extensions
let sandbox = Sandbox::builder()
    .with_package("numpy-wasi.tar.gz")?
    .build()?;
```

## Performance

| Metric | Normal WASM | Pre-compiled | Speedup |
|--------|-------------|--------------|---------|
| Sandbox creation | ~650ms | ~16ms | **41x faster** |
| Per-execution overhead | ~1.8ms | ~1.6ms | 14% faster |
| Session (5 executions) | ~70ms | ~3ms | **23x faster** |

## Development

This project uses [mise](https://mise.jdx.dev/) for tooling and task management.

### Setup

```bash
mise install
mise run setup  # Build WASM + precompile (one-time)
```

### Tasks

```bash
# Development
mise run check          # Run cargo check
mise run build          # Build all crates
mise run test           # Run tests with embedded WASM
mise run test-all       # Run tests with all features
mise run lint           # Run clippy lints
mise run fmt            # Format code
mise run fmt-check      # Check code formatting

# WASM
mise run build-eryx-runtime  # Build the Python WASM component
mise run build-all      # Build WASM + Rust crates
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

## Examples

```bash
cargo run --example simple              # Basic usage with TypedCallback
cargo run --example runtime_callbacks   # Runtime-defined callbacks (DynamicCallback)
cargo run --example with_tracing        # Execution tracing and output handling
cargo run --example error_handling      # Error handling scenarios
cargo run --example parallel_callbacks  # Parallel execution verification
cargo run --example custom_library      # Using RuntimeLibrary
cargo run --example session_reuse       # Session state persistence
cargo run --example resource_limits     # ResourceLimits usage
cargo run --example precompile --features embedded         # Pre-compilation demo
cargo run --example embedded_runtime --features embedded   # Embedded runtime
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
│   ├── eryx-runtime/       # Python WASM runtime packaging
│   │   ├── Cargo.toml
│   │   ├── build.rs        # Links eryx-wasm-runtime + libpython + WASI libs
│   │   ├── runtime.wit     # WIT interface definition
│   │   ├── runtime.wasm    # Built WASM component (~47MB)
│   │   ├── runtime.cwasm   # Pre-compiled native code (~52MB)
│   │   └── libs/           # WASI libraries (zstd compressed)
│   └── eryx-wasm-runtime/  # Rust runtime implementation (compiled to WASM)
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs      # WIT export implementations
│           └── python.rs   # Python interpreter FFI, tracing
└── docs/plans/             # Design documents
```

## License

MIT OR Apache-2.0
