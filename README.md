# Eryx

> **eryx** (noun): A genus of sand boas (Erycinae) — non-venomous snakes that live *in* sand.
> Perfect for "Python running inside a sandbox."

A Rust library that executes Python code in a WebAssembly sandbox with async callbacks.

## Features

- **Async callback mechanism** — Python can `await invoke("callback_name", ...)` to call host-provided functions
- **Parallel execution** — Multiple callbacks can run concurrently via `asyncio.gather()`
- **Execution tracing** — Line-level progress reporting via `sys.settrace`
- **Introspection** — Python can discover available callbacks at runtime
- **Composable runtime libraries** — Pre-built APIs with Python wrappers and type stubs

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

## With Callbacks

```rust
use eryx::{Callback, CallbackError, Sandbox};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

struct GetTime;

impl Callback for GetTime {
    fn name(&self) -> &str { "get_time" }
    fn description(&self) -> &str { "Returns the current Unix timestamp" }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn invoke(&self, _args: Value) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
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
        .build()?;

    let result = sandbox.execute(r#"
import asyncio
timestamp = asyncio.run(invoke("get_time", {}))
print(f"Current time: {timestamp}")
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

## Development

This project uses [mise](https://mise.jdx.dev/) for tooling and task management.

### Setup

```bash
mise install
```

### Tasks

```bash
mise run test       # Run tests with nextest
mise run lint       # Run clippy lints
mise run fmt        # Format code
mise run ci         # Run all CI checks (fmt-check, lint, test)
mise run doc        # Generate documentation
mise run doc-open   # Generate and open documentation
```

### Manual Commands

```bash
cargo nextest run --workspace           # Run tests
cargo clippy --workspace --all-targets  # Run lints
cargo fmt --all                         # Format code
cargo doc --workspace --no-deps --open  # Generate docs
```

## Project Structure

```
eryx/
├── Cargo.toml              # Workspace root
├── mise.toml               # mise configuration and tasks
├── .config/
│   └── nextest.toml        # nextest configuration
├── crates/
│   └── eryx/               # Core library crate
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs      # Public API exports
│           ├── sandbox.rs  # Sandbox struct, execute()
│           ├── callback.rs # Callback trait, CallbackError
│           ├── library.rs  # RuntimeLibrary struct
│           ├── trace.rs    # TraceEvent, TraceHandler
│           ├── wasm.rs     # wasmtime setup
│           └── error.rs    # Error types
└── plans/                  # Design documents
```

## License

MIT OR Apache-2.0