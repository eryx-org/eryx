# Resource Limits

Eryx provides fine-grained control over resource consumption to prevent runaway code from exhausting system resources. You can limit execution time, memory usage, callback invocations, and WASM instruction counts.

## Default Limits

By default, sandboxes have sensible limits applied:

| Resource | Default Value |
|----------|---------------|
| Execution timeout | 30 seconds |
| Callback timeout | 10 seconds |
| Memory | 128 MB |
| Callback invocations | 1000 |

## Configuring Resource Limits

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_timeout(Duration::from_secs(5))
        .with_memory_limit(64 * 1024 * 1024)  // 64 MB
        .build()?;

    let result = sandbox.execute("print('hello')").await?;
    println!("{}", result.stdout);

    Ok(())
}
```

```python
import eryx

# Configure custom limits
limits = eryx.ResourceLimits(
    execution_timeout_ms=5000,      # 5 seconds
    callback_timeout_ms=2000,       # 2 seconds per callback
    max_memory_bytes=64_000_000,    # 64 MB
    max_callback_invocations=100,   # Max 100 callback calls
)

sandbox = eryx.Sandbox(resource_limits=limits)
result = sandbox.execute("print('hello')")
print(result.stdout)
```
<!-- langtabs-end -->

## Execution Timeout

The execution timeout limits how long Python code can run before being terminated:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, Error};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_timeout(Duration::from_millis(500))
        .build()?;

    match sandbox.execute("while True: pass").await {
        Err(Error::Timeout { duration, .. }) => {
            println!("Timed out after {:?}", duration);
        }
        _ => {}
    }

    Ok(())
}
```

```python
import eryx

limits = eryx.ResourceLimits(execution_timeout_ms=500)
sandbox = eryx.Sandbox(resource_limits=limits)

try:
    sandbox.execute("while True: pass")
except eryx.TimeoutError:
    print("Execution timed out")
```
<!-- langtabs-end -->

## Callback Timeout

When sandboxed code calls host callbacks, you can limit how long each callback can take:

```python
import eryx
import time

def slow_callback():
    time.sleep(5)  # This will exceed the callback timeout
    return {"result": "done"}

limits = eryx.ResourceLimits(callback_timeout_ms=1000)  # 1 second per callback

sandbox = eryx.Sandbox(
    resource_limits=limits,
    callbacks=[
        {"name": "slow", "fn": slow_callback, "description": "A slow callback"}
    ]
)

try:
    sandbox.execute("await slow()")
except eryx.TimeoutError:
    print("Callback timed out")
```

## Memory Limits

Control the maximum memory the WebAssembly instance can use:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_memory_limit(32 * 1024 * 1024)  // 32 MB
        .build()?;

    // This will fail if it tries to allocate too much memory
    let result = sandbox.execute(r#"
# Try to allocate a large list
try:
    big_list = [0] * 10_000_000
    print("Allocated successfully")
except MemoryError:
    print("Memory limit exceeded")
    "#).await?;

    println!("{}", result.stdout);

    Ok(())
}
```

```python
import eryx

limits = eryx.ResourceLimits(max_memory_bytes=32_000_000)  # 32 MB
sandbox = eryx.Sandbox(resource_limits=limits)

result = sandbox.execute("""
try:
    big_list = [0] * 10_000_000
    print("Allocated successfully")
except MemoryError:
    print("Memory limit exceeded")
""")
print(result.stdout)
```
<!-- langtabs-end -->

## Callback Invocation Limits

Limit the total number of callback invocations to prevent abuse:

```python
import eryx

def noop():
    return {}

limits = eryx.ResourceLimits(max_callback_invocations=5)

sandbox = eryx.Sandbox(
    resource_limits=limits,
    callbacks=[{"name": "noop", "fn": noop, "description": ""}]
)

result = sandbox.execute("""
for i in range(10):
    try:
        await noop()
        print(f"Call {i+1} succeeded")
    except Exception as e:
        print(f"Call {i+1} failed: {type(e).__name__}")
        break
""")
print(result.stdout)
```

## Fuel Limits (Instruction Counting)

Fuel limits provide fine-grained control over execution by limiting the number of WebAssembly instructions that can be executed. This is useful for:

- Deterministic execution bounds (time limits vary with system load, fuel doesn't)
- Billing based on actual computation performed
- Preventing CPU-intensive attacks

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // Set a fuel limit on the session
    session.set_fuel_limit(Some(500_000_000));  // 500M instructions

    // Simple operations succeed
    let result = session.execute("x = 1 + 1").await?;
    println!("Fuel consumed: {:?}", result.stats.fuel_consumed);

    // Reset for next test
    session.reset(&[]).await?;

    // Large loops may exhaust fuel
    let result = session
        .execute("for i in range(1000000): pass")
        .await;

    match result {
        Err(eryx::Error::FuelExhausted { consumed, limit }) => {
            println!("Fuel exhausted: consumed {}, limit {}", consumed, limit);
        }
        Ok(r) => println!("Completed with fuel: {:?}", r.stats.fuel_consumed),
        Err(e) => println!("Other error: {}", e),
    }

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("x = sum(range(1000))")

# Fuel consumed is reported in execution results
print(f"Fuel consumed: {result.fuel_consumed}")
```
<!-- langtabs-end -->

### Per-Execution Fuel Limits

You can set fuel limits per-execution to override session defaults:

```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // Session has generous limit
    session.set_fuel_limit(Some(1_000_000_000));

    // But this execution has a tight limit
    let result = session
        .execute("for i in range(1000000): pass")
        .with_fuel_limit(100_000_000)  // Override with tighter limit
        .await;

    // Will fail due to per-execution limit
    assert!(result.is_err());

    Ok(())
}
```

### Fuel Consumption is Deterministic

The same code always consumes the same amount of fuel:

```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    let code = "x = sum(range(100))";

    // Run multiple times
    let mut fuel_values = Vec::new();
    for _ in 0..3 {
        let mut session = InProcessSession::new(&sandbox).await?;
        let result = session.execute(code).await?;
        fuel_values.push(result.stats.fuel_consumed.unwrap());
    }

    // All runs consume the same fuel
    assert!(fuel_values.iter().all(|&f| f == fuel_values[0]));
    println!("Consistent fuel consumption: {:?}", fuel_values[0]);

    Ok(())
}
```

## Unlimited Resources

For trusted code or development environments, you can disable limits:

```python
import eryx

# Remove all limits (use with caution!)
limits = eryx.ResourceLimits.unlimited()

sandbox = eryx.Sandbox(resource_limits=limits)
result = sandbox.execute("print('no limits!')")
print(result.stdout)
```

## Resource Usage Reporting

Execution results include resource usage statistics:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    let result = sandbox.execute("x = [i for i in range(10000)]").await?;

    println!("Duration: {:?}", result.stats.duration);
    println!("Peak memory: {:?} bytes", result.stats.peak_memory_bytes);
    println!("Fuel consumed: {:?}", result.stats.fuel_consumed);

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("x = [i for i in range(10000)]")

print(f"Duration: {result.duration_ms}ms")
print(f"Peak memory: {result.peak_memory_bytes} bytes")
print(f"Callback invocations: {result.callback_invocations}")
```
<!-- langtabs-end -->

## Recovery After Resource Exhaustion

Sessions can recover after hitting resource limits by resetting:

```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.set_fuel_limit(Some(100_000_000));

    // First execution exhausts fuel
    let result = session.execute("for i in range(1000000): pass").await;
    assert!(result.is_err());

    // Reset the session
    session.reset(&[]).await?;

    // Session works again
    let result = session.execute("print('recovered')").await?;
    println!("{}", result.stdout);  // "recovered"

    Ok(())
}
```

## Best Practices

1. **Always set timeouts** - Prevent infinite loops from hanging your application
2. **Use appropriate memory limits** - Consider your use case and available system resources
3. **Monitor fuel consumption** - Use fuel stats to understand computational costs
4. **Limit callbacks** - Especially if callbacks have side effects or external costs
5. **Test with limits** - Ensure your code works within the constraints you set

## Next Steps

- [Sandboxes](./sandboxes.md) - Creating and using sandboxes
- [Sessions](./sessions.md) - Stateful execution with resource limits
- [Networking](./networking.md) - Network configuration and limits
