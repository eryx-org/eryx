# Quick Start

This guide will help you create your first Eryx sandbox and execute Python code.

## Basic Execution

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Create a sandbox with embedded Python runtime
    let sandbox = Sandbox::embedded().build()?;

    // Execute Python code
    let result = sandbox.execute(r#"
print("Hello from Python!")
x = 2 + 2
print(f"2 + 2 = {x}")
    "#).await?;

    println!("{}", result.stdout);
    // Output:
    // Hello from Python!
    // 2 + 2 = 4

    println!("Execution took {:.2}ms", result.stats.duration.as_secs_f64() * 1000.0);

    Ok(())
}
```

```python
import eryx

# Create a sandbox with embedded Python runtime
sandbox = eryx.Sandbox()

# Execute Python code in isolation
result = sandbox.execute('''
print("Hello from the sandbox!")
x = 2 + 2
print(f"2 + 2 = {x}")
''')

print(result.stdout)
# Output:
# Hello from the sandbox!
# 2 + 2 = 4

print(f"Execution took {result.duration_ms:.2f}ms")
```
<!-- langtabs-end -->

## With Callbacks

Callbacks allow sandboxed code to interact with the host in a controlled way.

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use eryx::{callback, CallbackError, Sandbox};
use serde_json::{json, Value};

/// Echoes back the message
#[callback]
async fn echo(message: String) -> Result<Value, CallbackError> {
    Ok(json!({ "echoed": message }))
}

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_callback(echo)
        .build()?;

    let result = sandbox.execute(r#"
response = await echo(message="Hello!")
print(f"Echo: {response}")
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}
```

```python
import eryx

registry = eryx.CallbackRegistry()

@registry.callback(description="Returns current Unix timestamp")
def get_time():
    import time
    return {"timestamp": time.time()}

sandbox = eryx.Sandbox(callbacks=registry)

result = sandbox.execute("""
t = await get_time()
print(f"Time: {t['timestamp']}")
""")

print(result.stdout)
```
<!-- langtabs-end -->

> **Note:** The Rust `#[callback]` macro requires the `macros` feature flag. See [Installation](./installation.md#feature-flags).

## With Sessions

Sessions maintain state across multiple executions, useful for REPL-style usage.

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // State persists across executions
    session.execute("x = 42").await?;
    let result = session.execute("print(x * 2)").await?;
    println!("{}", result.stdout); // "84"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

# State persists across executions
session.execute("x = 42")
session.execute("y = x * 2")
result = session.execute("print(f'{x} * 2 = {y}')")
print(result.stdout)  # "42 * 2 = 84"
```
<!-- langtabs-end -->

## Performance

Both Rust and Python bindings use pre-compiled WebAssembly for fast sandbox creation:

| Metric | Normal Wasm | Pre-compiled | Speedup |
|--------|-------------|--------------|---------|
| Sandbox creation | ~650ms | ~16ms | **41x faster** |
| Per-execution overhead | ~1.8ms | ~1.6ms | 14% faster |
| Session (5 executions) | ~70ms | ~3ms | **23x faster** |

> **Rust Note:** The `embedded` feature flag enables pre-compilation but requires a one-time setup step. See [Installation](./installation.md#setting-up-the-embedded-feature) for details.

## Next Steps

- [Core Concepts](./core-concepts.md) - Understand Eryx architecture
- [Sandboxes Guide](../guide/sandboxes.md) - Learn about sandbox configuration
- [Callbacks Guide](../guide/callbacks.md) - Deep dive into callbacks
- [Sessions Guide](../guide/sessions.md) - Master stateful execution
