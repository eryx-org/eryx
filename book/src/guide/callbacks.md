# Callbacks

Callbacks allow sandboxed Python code to call functions on the host. This enables controlled interaction with external resources like databases, APIs, or system services while maintaining the security of the sandbox.

## Overview

From Python's perspective, callbacks appear as async functions that can be awaited:

<!-- langtabs-start -->

```python
# Inside the sandbox
result = await get_weather(city="London")
print(result["temperature"])
```

<!-- langtabs-end -->

The host defines these callbacks and controls exactly what the sandbox can access.

## Defining Callbacks

<!-- langtabs-start -->

### Rust (with macro)

The `#[callback]` macro provides the simplest way to define callbacks. Enable it with the `macros` feature:

```toml
[dependencies]
eryx = { version = "0.3", features = ["embedded", "macros"] }
```

```rust
# extern crate eryx;
# extern crate tokio;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use eryx::{callback, CallbackError, Sandbox};
use serde_json::{json, Value};

/// Returns the current Unix timestamp
#[callback]
async fn get_time() -> Result<Value, CallbackError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
        .as_secs();
    Ok(json!({ "timestamp": now }))
}

/// Echoes the message back with optional repetition
#[callback]
async fn echo(message: String, repeat: Option<u32>) -> Result<Value, CallbackError> {
    let repeat = repeat.unwrap_or(1) as usize;
    Ok(json!({ "echoed": message.repeat(repeat) }))
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_callback(get_time)
        .with_callback(echo)
        .build()?;

    let result = sandbox.execute(r#"
t = await get_time()
print(f"Time: {t['timestamp']}")

msg = await echo(message="hi", repeat=3)
print(msg['echoed'])  # "hihihi"
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

The macro automatically:
- Creates a unit struct with the function name (e.g., `get_time`)
- Uses the doc comment as the callback description
- Generates a JSON Schema from the function parameters
- Marks `Option<T>` parameters as optional

### Python (with decorator)

Use the `CallbackRegistry` decorator for the cleanest syntax:

```python
import eryx

registry = eryx.CallbackRegistry()

@registry.callback(description="Returns current Unix timestamp")
def get_time():
    import time
    return {"timestamp": time.time()}

@registry.callback(description="Echoes the message back")
def echo(message: str, repeat: int = 1):
    return {"echoed": message * repeat}

sandbox = eryx.Sandbox(callbacks=registry)

result = sandbox.execute("""
t = await get_time()
print(f"Time: {t['timestamp']}")

msg = await echo(message="hi", repeat=3)
print(msg['echoed'])  # "hihihi"
""")
```

<!-- langtabs-end -->

## Parameters and Types

Callback parameters are passed as JSON from the sandbox. Parameters must be deserializable types.

<!-- langtabs-start -->

### Rust

```rust
# extern crate eryx;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use eryx::{callback, CallbackError};
use serde_json::{json, Value};

/// Searches for users matching the criteria
#[callback]
async fn search_users(
    query: String,              // Required parameter
    limit: Option<u32>,         // Optional with default (via unwrap_or)
    include_inactive: Option<bool>,
) -> Result<Value, CallbackError> {
    let limit = limit.unwrap_or(10);
    let _include_inactive = include_inactive.unwrap_or(false);

    // ... perform search ...
    let _ = query;
    Ok(json!({ "users": [], "total": 0 }))
}
```

### Python

```python,no_test
@registry.callback(description="Searches for users")
def search_users(
    query: str,
    limit: int = 10,
    include_inactive: bool = False
):
    # ... perform search ...
    return {"users": [], "total": 0}
```

<!-- langtabs-end -->

## Error Handling

Return errors to indicate failures to the sandbox.

<!-- langtabs-start -->

### Rust

```rust
# extern crate eryx;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use eryx::{callback, CallbackError};
use serde_json::{json, Value};

/// Divides two numbers
#[callback]
async fn divide(a: f64, b: f64) -> Result<Value, CallbackError> {
    if b == 0.0 {
        return Err(CallbackError::InvalidArguments("Cannot divide by zero".into()));
    }
    Ok(json!({ "result": a / b }))
}
```

Error variants:
- `CallbackError::InvalidArguments(msg)` - Bad input from the sandbox
- `CallbackError::ExecutionFailed(msg)` - Internal host error

### Python

```python,no_test
@registry.callback(description="Divides two numbers")
def divide(a: float, b: float):
    if b == 0:
        raise ValueError("Cannot divide by zero")
    return {"result": a / b}
```

<!-- langtabs-end -->

## Parallel Execution

Multiple callbacks can run concurrently using `asyncio.gather()`:

<!-- langtabs-start -->

```python,no_test
# Inside the sandbox
import asyncio

# These run in parallel on the host
results = await asyncio.gather(
    fetch_user(id=1),
    fetch_user(id=2),
    fetch_posts(user_id=1),
)
user1, user2, posts = results
```

<!-- langtabs-end -->

## Introspection

The sandbox can discover available callbacks at runtime:

<!-- langtabs-start -->

```python,no_test
# Inside the sandbox
import eryx_callbacks

# List all available callbacks
for name in dir(eryx_callbacks):
    callback = getattr(eryx_callbacks, name)
    print(f"{name}: {callback.__doc__}")
```

<!-- langtabs-end -->

## Alternative APIs

### Rust: Manual TypedCallback

For more control, implement `TypedCallback` directly:

<!-- langtabs-start -->

```rust
# extern crate eryx;
# extern crate tokio;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use std::{future::Future, pin::Pin};
use eryx::{TypedCallback, CallbackError, Sandbox, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, JsonSchema)]
struct GreetArgs {
    name: String,
    #[serde(default)]
    formal: bool,
}

struct Greet;

impl TypedCallback for Greet {
    type Args = GreetArgs;

    fn name(&self) -> &str { "greet" }
    fn description(&self) -> &str { "Generates a greeting" }

    fn invoke_typed(
        &self,
        args: GreetArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let greeting = if args.formal {
                format!("Good day, {}.", args.name)
            } else {
                format!("Hey {}!", args.name)
            };
            Ok(json!({ "greeting": greeting }))
        })
    }
}

# #[tokio::main]
# async fn main() -> Result<(), eryx::Error> {
let sandbox = Sandbox::embedded()
    .with_callback(Greet)
    .build()?;
#     Ok(())
# }
```

<!-- langtabs-end -->

### Rust: DynamicCallback

For runtime-defined callbacks without compile-time types:

<!-- langtabs-start -->

```rust
# extern crate eryx;
# extern crate tokio;
# extern crate serde_json;
use eryx::{DynamicCallback, Sandbox};
use serde_json::json;

# #[tokio::main]
# async fn main() -> Result<(), eryx::Error> {
let callback = DynamicCallback::builder(
    "get_config",
    "Returns application configuration",
    |_args| Box::pin(async move {
        Ok(json!({
            "version": "1.0.0",
            "debug": false
        }))
    })
).build();

let sandbox = Sandbox::embedded()
    .with_callback(callback)
    .build()?;
#     Ok(())
# }
```

<!-- langtabs-end -->

### Python: Dict API

For simple cases, use a list of dictionaries:

<!-- langtabs-start -->

```python
import eryx

def my_callback(args):
    return {"result": args.get("value", 0) * 2}

sandbox = eryx.Sandbox(
    callbacks=[
        {
            "name": "double",
            "fn": my_callback,
            "description": "Doubles a value"
        }
    ]
)
```

<!-- langtabs-end -->

## Best Practices

1. **Keep callbacks focused** - Each callback should do one thing well
2. **Validate inputs** - Don't trust data from the sandbox
3. **Handle errors gracefully** - Return meaningful error messages
4. **Use descriptive names** - Callbacks appear as functions in Python
5. **Document with descriptions** - Help sandbox code understand what's available
6. **Consider timeouts** - Long-running callbacks can block sandbox execution
