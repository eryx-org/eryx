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

Use the `#[callback]` macro for strongly-typed callbacks with automatic JSON Schema generation:

```rust
use eryx::{callback, CallbackError, Sandbox};
use serde_json::{json, Value};

/// Returns the current Unix timestamp
#[callback]
async fn get_time() -> Result<Value, CallbackError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    Ok(json!(now))
}

/// Echoes back the message
#[callback]
async fn echo(message: String) -> Result<Value, CallbackError> {
    Ok(json!({ "echoed": message }))
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::builder()
        .with_callback(get_time)
        .with_callback(echo)
        .build()?;

    let result = sandbox.execute(r#"
timestamp = await get_time()
print(f"Current time: {timestamp}")

response = await echo(message="Hello!")
print(f"Echo: {response}")
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

## Dynamic Callbacks

For runtime-defined callbacks (e.g., from configuration or plugins):

```rust
use eryx::{DynamicCallback, Sandbox, CallbackError};
use serde_json::json;

let greet = DynamicCallback::builder("greet", "Greets a person", |args| {
        Box::pin(async move {
            let name = args["name"].as_str().unwrap_or("stranger");
            Ok(json!({ "greeting": format!("Hello, {}!", name) }))
        })
    })
    .param("name", "string", "The person's name", true)
    .build();

let sandbox = Sandbox::builder()
    .with_callback(greet)
    .build()?;
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
