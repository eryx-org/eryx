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

## With Callbacks (TypedCallback)

Use `TypedCallback` for strongly-typed callbacks with automatic JSON Schema generation:

```rust
use std::{future::Future, pin::Pin};

use eryx::{TypedCallback, CallbackError, Sandbox, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

// Define typed arguments - schema is auto-generated
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
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

// For no-argument callbacks, use () as Args
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
