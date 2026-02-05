# Callbacks

Callbacks are the primary mechanism for sandboxed Python code to interact with the host environment. They enable controlled access to external resources like databases, APIs, or system services while maintaining sandbox isolation.

## How Callbacks Work

When Python code calls a callback function (e.g., `await get_time()`), the execution flows from the sandbox to your host code:

1. Python code calls the callback function
2. Arguments are serialized to JSON and sent to the host
3. Your callback executes outside the sandbox
4. The result is serialized and returned to Python

Callbacks are always asynchronous from Python's perspective, so they must be called with `await`.

## Python: Decorator API

The recommended way to define callbacks in Python is using the `CallbackRegistry` decorator:

```python
import eryx

registry = eryx.CallbackRegistry()

@registry.callback(description="Returns the current Unix timestamp")
def get_time():
    import time
    return {"timestamp": time.time()}

@registry.callback(description="Echoes the message back")
def echo(message: str, repeat: int = 1):
    return {"echoed": message * repeat}

sandbox = eryx.Sandbox(callbacks=registry)

result = sandbox.execute("""
t = await get_time()
print(f"Time: {t['timestamp']:.2f}")

response = await echo(message="Hello! ", repeat=3)
print(f"Echo: {response['echoed']}")
""")
```

### Decorator Options

The `@registry.callback()` decorator accepts these parameters:

| Parameter | Type | Description |
|-----------|------|-------------|
| `name` | `str` | Override the callback name (defaults to function name) |
| `description` | `str` | Human-readable description for introspection |
| `schema` | `dict` | Custom JSON Schema for parameters |

```python
@registry.callback(
    name="calc",  # Override function name
    description="Performs arithmetic operations"
)
def do_calculation(operation: str, a: float, b: float):
    ops = {"add": a + b, "sub": a - b, "mul": a * b, "div": a / b}
    return {"result": ops.get(operation, 0)}
```

### Schema Inference

Type hints are automatically converted to JSON Schema:

| Python Type | JSON Schema Type |
|-------------|------------------|
| `str` | `"string"` |
| `int` | `"integer"` |
| `float` | `"number"` |
| `bool` | `"boolean"` |
| `list` | `"array"` |
| `dict` | `"object"` |

### Async Callbacks

Both sync and async functions are supported. Async functions are detected automatically:

```python
import asyncio

@registry.callback(description="Async operation with delay")
async def fetch_data(url: str):
    await asyncio.sleep(0.1)  # Simulate network delay
    return {"data": f"Response from {url}"}
```

> **Note:** Each async callback invocation runs in an isolated event loop. This means asyncio primitives (Queue, Lock, Semaphore) cannot be shared between callbacks. Use thread-safe alternatives (threading.Lock, queue.Queue) if you need shared state.

## Python: Dict API

For dynamic callback registration, you can pass a list of dictionaries:

```python
def get_time():
    import time
    return {"timestamp": time.time()}

sandbox = eryx.Sandbox(
    callbacks=[
        {
            "name": "get_time",
            "fn": get_time,
            "description": "Returns current Unix timestamp",
            # Optional: "schema": {...}
        }
    ]
)
```

This is useful when callbacks are discovered at runtime or loaded from configuration.

## Rust: TypedCallback

For Rust, the `TypedCallback` trait provides compile-time type safety with automatic schema generation:

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
struct EchoArgs {
    /// The message to echo back
    message: String,
    /// Number of times to repeat (optional)
    #[serde(default)]
    repeat: Option<u32>,
}

struct Echo;

impl TypedCallback for Echo {
    type Args = EchoArgs;

    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echoes back the message" }

    fn invoke_typed(
        &self,
        args: EchoArgs
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let repeat = args.repeat.unwrap_or(1);
            let echoed: String = std::iter::repeat(args.message.as_str())
                .take(repeat as usize)
                .collect::<Vec<_>>()
                .join(" ");
            Ok(json!({ "echoed": echoed }))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_callback(Echo)
        .build()?;

    let result = sandbox.execute(r#"
response = await echo(message="Hello!", repeat=3)
print(response)
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

### No-Argument Callbacks

For callbacks without arguments, use `()` as the `Args` type:

```rust,ignore
struct GetTime;

impl TypedCallback for GetTime {
    type Args = ();

    fn name(&self) -> &str { "get_time" }
    fn description(&self) -> &str { "Returns current Unix timestamp" }

    fn invoke_typed(
        &self,
        _args: ()
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Ok(json!({ "timestamp": now }))
        })
    }
}
```

## Rust: DynamicCallback

For runtime-defined callbacks or when you don't need compile-time type checking, use `DynamicCallback`:

```rust,ignore
use eryx::{DynamicCallback, CallbackError};
use serde_json::json;

let greet = DynamicCallback::builder("greet", "Greets a person", |args| {
    Box::pin(async move {
        let name = args.get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CallbackError::InvalidArguments("missing 'name'".into()))?;

        let formal = args.get("formal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let greeting = if formal {
            format!("Good day, {}.", name)
        } else {
            format!("Hey, {}!", name)
        };

        Ok(json!({ "greeting": greeting }))
    })
})
.param("name", "string", "The person's name", true)
.param("formal", "boolean", "Use formal greeting", false)
.build();

let sandbox = Sandbox::embedded()
    .with_callback(greet)
    .build()?;
```

### Builder Methods

| Method | Description |
|--------|-------------|
| `.param(name, type, description, required)` | Add a parameter to the schema |
| `.schema(schema)` | Override with a custom JSON Schema |
| `.build()` | Construct the callback |

Supported types for `.param()`: `"string"`, `"number"`, `"integer"`, `"boolean"`, `"object"`, `"array"`.

## Error Handling

Errors thrown in callbacks propagate to the Python code as exceptions:

<!-- langtabs-start -->
```rust,ignore
impl TypedCallback for MayFail {
    // ...
    fn invoke_typed(&self, args: Args) -> ... {
        Box::pin(async move {
            if args.should_fail {
                return Err(CallbackError::ExecutionFailed(
                    "Something went wrong".into()
                ));
            }
            Ok(json!({"status": "ok"}))
        })
    }
}
```

```python
@registry.callback(description="A callback that may fail")
def may_fail(should_fail: bool = False):
    if should_fail:
        raise ValueError("Something went wrong")
    return {"status": "ok"}
```
<!-- langtabs-end -->

The Python code can catch these exceptions:

```python
result = sandbox.execute("""
try:
    await may_fail(should_fail=True)
except Exception as e:
    print(f"Caught: {e}")
""")
```

### Error Types

Rust callbacks can return these error variants:

| Error | Use Case |
|-------|----------|
| `CallbackError::InvalidArguments(msg)` | Bad arguments (type mismatch, missing required) |
| `CallbackError::ExecutionFailed(msg)` | Logic/runtime errors during execution |
| `CallbackError::NotFound(name)` | Callback doesn't exist (internal) |
| `CallbackError::Timeout` | Callback exceeded time limit |

## Introspection

Sandboxed code can discover available callbacks at runtime:

```python
result = sandbox.execute("""
callbacks = list_callbacks()
for cb in callbacks:
    print(f"{cb['name']}: {cb['description']}")
    if 'parameters_schema' in cb:
        print(f"  Schema: {cb['parameters_schema']}")
""")
```

This is useful for:
- Dynamic UIs that adapt to available callbacks
- LLM agents that need to know what tools are available
- Debugging and development

## Parallel Execution

Callbacks can run concurrently using `asyncio.gather()`:

```python
result = sandbox.execute("""
import asyncio

# These run in parallel on the host
results = await asyncio.gather(
    fetch_data(url="http://api1.example.com"),
    fetch_data(url="http://api2.example.com"),
    fetch_data(url="http://api3.example.com"),
)

for r in results:
    print(r)
""")
```

Eryx executes parallel callbacks concurrently using Tokio, so I/O-bound callbacks benefit from true parallelism.

## Stateful Callbacks

Callbacks can maintain state using closures:

<!-- langtabs-start -->
```rust,ignore
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

let counter = Arc::new(AtomicU32::new(0));
let counter_clone = counter.clone();

let increment = DynamicCallback::builder("increment", "Increments counter", move |args| {
    let counter = counter_clone.clone();
    Box::pin(async move {
        let amount = args.get("amount")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;
        let new_value = counter.fetch_add(amount, Ordering::SeqCst) + amount;
        Ok(json!({ "count": new_value }))
    })
})
.param("amount", "integer", "Amount to add", false)
.build();
```

```python
def create_counter(initial: int = 0):
    state = {"count": initial}

    @registry.callback(description="Increments the counter")
    def increment(amount: int = 1):
        state["count"] += amount
        return {"count": state["count"]}

    return increment

create_counter(10)  # Registers with initial value 10
```
<!-- langtabs-end -->

## Resource Limits

You can limit callback behavior through `ResourceLimits`:

<!-- langtabs-start -->
```rust,ignore
use eryx::{Sandbox, ResourceLimits};
use std::time::Duration;

let sandbox = Sandbox::embedded()
    .with_callback(MyCallback)
    .with_resource_limits(ResourceLimits {
        max_callback_invocations: Some(100),
        callback_timeout: Some(Duration::from_secs(5)),
        ..Default::default()
    })
    .build()?;
```

```python
sandbox = eryx.Sandbox(
    callbacks=registry,
    resource_limits=eryx.ResourceLimits(
        max_callback_invocations=100,
        callback_timeout_ms=5000,
    )
)
```
<!-- langtabs-end -->

| Limit | Description |
|-------|-------------|
| `max_callback_invocations` | Maximum total callback calls per execution |
| `callback_timeout` | Maximum time per individual callback |

## Best Practices

1. **Keep callbacks focused** - Each callback should do one thing well
2. **Use descriptive names** - Names become Python functions, so use snake_case
3. **Document with descriptions** - Descriptions help with introspection and LLM usage
4. **Handle errors gracefully** - Return meaningful error messages
5. **Prefer TypedCallback** - Compile-time safety catches bugs early
6. **Use DynamicCallback for plugins** - When callback definitions come from config or discovery

## Next Steps

- [Sessions](./sessions.md) - Maintain state across executions
- [Resource Limits](./resource-limits.md) - Configure execution constraints
- [API Reference (Python)](../api/python.md) - Full Python API documentation
- [API Reference (Rust)](../api/rust.md) - Full Rust API documentation
