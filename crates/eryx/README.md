# Eryx

> **eryx** (noun): A genus of sand boas (Erycinae) - non-venomous snakes that live *in* sand.
> Perfect for "Python running inside a sandbox."

A Python sandbox with async callbacks powered by WebAssembly.

## Features

- **Async callback mechanism** - Callbacks are exposed as direct async functions (e.g., `await get_time()`)
- **Parallel execution** - Multiple callbacks can run concurrently via `asyncio.gather()`
- **Execution tracing** - Line-level progress reporting via `sys.settrace`
- **Introspection** - Python can discover available callbacks at runtime
- **Composable runtime libraries** - Pre-built APIs with Python wrappers and type stubs
- **LLM-friendly** - Type stubs (`.pyi`) for including in context windows

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
    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Returns the current Unix timestamp"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn invoke(
        &self,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
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
# Callbacks are available as direct async functions
timestamp = await get_time()
print(f"Current time: {timestamp}")
    "#).await?;

    println!("Output: {}", result.stdout);
    Ok(())
}
```

## With Runtime Libraries

Runtime libraries bundle callbacks with Python wrappers and type stubs:

```rust
use eryx::{RuntimeLibrary, Sandbox};

let library = RuntimeLibrary::new()
    .with_callback(MyCallback)
    .with_preamble(include_str!("preamble.py"))
    .with_stubs(include_str!("stubs.pyi"));

let sandbox = Sandbox::builder()
    .with_library(library)
    .build()?;
```

## License

MIT OR Apache-2.0