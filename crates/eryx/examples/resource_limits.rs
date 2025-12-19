//! Example demonstrating resource limits for sandbox execution.
//!
//! This example shows how to configure and use `ResourceLimits` to control:
//! - Execution timeout (maximum time for the entire script)
//! - Callback timeout (maximum time for a single callback)
//! - Callback invocation limits (maximum number of callback calls)
//!
//! Run with: `cargo run --example resource_limits --release`

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use eryx::JsonSchema;
use eryx::{CallbackError, ResourceLimits, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

/// Arguments for the sleep callback.
#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Milliseconds to sleep
    ms: u64,
}

/// A callback that sleeps for a specified duration.
struct Sleep;

impl TypedCallback for Sleep {
    type Args = SleepArgs;

    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleeps for the specified number of milliseconds"
    }

    fn invoke_typed(
        &self,
        args: SleepArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(args.ms)).await;
            Ok(json!({"slept_ms": args.ms}))
        })
    }
}

/// Arguments for the counter callback.
#[derive(Deserialize, JsonSchema)]
struct CountArgs {
    /// The count value to echo back
    n: i64,
}

/// A simple counter callback for demonstrating invocation limits.
struct Counter;

impl TypedCallback for Counter {
    type Args = CountArgs;

    fn name(&self) -> &str {
        "count"
    }

    fn description(&self) -> &str {
        "Returns an incrementing count (stateless, just returns the input)"
    }

    fn invoke_typed(
        &self,
        args: CountArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"count": args.n})) })
    }
}

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    println!("=== Resource Limits Example ===\n");

    // Example 1: Callback invocation limit
    println!("--- Example 1: Callback Invocation Limit ---");
    println!("Limiting to 3 callback invocations...\n");

    let sandbox = Sandbox::embedded()
        .with_callback(Counter)
        .with_resource_limits(ResourceLimits {
            max_callback_invocations: Some(3),
            ..Default::default()
        })
        .build()?;

    let result = rt.block_on(async {
        sandbox
            .execute(
                r#"
results = []
for i in range(5):  # Try to invoke 5 times, but limit is 3
    try:
        result = await count(n=i)
        results.append(f"Success: {result}")
    except Exception as e:
        results.append(f"Error: {e}")

print("\n".join(results))
"#,
            )
            .await
    })?;

    println!("Output:\n{}", result.stdout);
    println!(
        "Callback invocations recorded: {}\n",
        result.stats.callback_invocations
    );

    // Example 2: Execution timeout
    println!("--- Example 2: Execution Timeout ---");
    println!("Setting 2 second execution timeout...\n");

    let sandbox = Sandbox::embedded()
        .with_callback(Sleep)
        .with_resource_limits(ResourceLimits {
            execution_timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        })
        .build()?;

    let start = std::time::Instant::now();
    let result = rt.block_on(async {
        sandbox
            .execute(
                r#"
# This should complete - only 500ms sleep
result = await sleep(ms=500)
print(f"First sleep completed: {result}")
"#,
            )
            .await
    });

    match result {
        Ok(r) => println!("Completed in {:?}:\n{}", start.elapsed(), r.stdout),
        Err(e) => println!("Error after {:?}: {}", start.elapsed(), e),
    }

    // Example 3: Callback timeout
    println!("\n--- Example 3: Callback Timeout ---");
    println!("Setting 500ms callback timeout...\n");

    let sandbox = Sandbox::embedded()
        .with_callback(Sleep)
        .with_resource_limits(ResourceLimits {
            callback_timeout: Some(Duration::from_millis(500)),
            execution_timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        })
        .build()?;

    let result = rt.block_on(async {
        sandbox
            .execute(
                r#"
# First call: 100ms - should succeed
try:
    result = await sleep(ms=100)
    print(f"100ms sleep: Success - {result}")
except Exception as e:
    print(f"100ms sleep: Error - {e}")

# Second call: 1000ms - should timeout (limit is 500ms)
try:
    result = await sleep(ms=1000)
    print(f"1000ms sleep: Success - {result}")
except Exception as e:
    print(f"1000ms sleep: Error - {e}")
"#,
            )
            .await
    })?;

    println!("Output:\n{}", result.stdout);

    // Example 4: Custom limits for untrusted code
    println!("\n--- Example 4: Restrictive Limits for Untrusted Code ---");

    let restrictive_limits = ResourceLimits {
        execution_timeout: Some(Duration::from_secs(5)),
        callback_timeout: Some(Duration::from_secs(1)),
        max_memory_bytes: Some(64 * 1024 * 1024), // 64 MB
        max_callback_invocations: Some(10),
    };

    println!("Configured limits:");
    println!(
        "  - Execution timeout: {:?}",
        restrictive_limits.execution_timeout
    );
    println!(
        "  - Callback timeout: {:?}",
        restrictive_limits.callback_timeout
    );
    println!(
        "  - Max memory: {:?} bytes",
        restrictive_limits.max_memory_bytes
    );
    println!(
        "  - Max callback invocations: {:?}",
        restrictive_limits.max_callback_invocations
    );

    let sandbox = Sandbox::embedded()
        .with_callback(Counter)
        .with_resource_limits(restrictive_limits)
        .build()?;

    let result = rt.block_on(async {
        sandbox
            .execute(
                r#"
# Simple trusted code running with restrictive limits
for i in range(5):
    result = await count(n=i)
    print(f"Count {i}: {result}")
"#,
            )
            .await
    })?;

    println!("\nOutput:\n{}", result.stdout);
    println!("Execution stats:");
    println!("  - Duration: {:?}", result.stats.duration);
    println!(
        "  - Callback invocations: {}",
        result.stats.callback_invocations
    );
    println!(
        "  - Peak memory: {:?}",
        result
            .stats
            .peak_memory_bytes
            .map(|b| format!("{} bytes", b))
            .unwrap_or_else(|| "not tracked".to_string())
    );

    // Example 5: No limits (use with caution!)
    println!("\n--- Example 5: No Limits (Dangerous!) ---");

    let no_limits = ResourceLimits {
        execution_timeout: None,
        callback_timeout: None,
        max_memory_bytes: None,
        max_callback_invocations: None,
    };

    println!("⚠️  All limits disabled - use only for trusted code!");
    println!("Configured limits: {:?}", no_limits);

    println!("\n=== Summary ===");
    println!("ResourceLimits fields:");
    println!("  - execution_timeout: Maximum time for entire script execution");
    println!("  - callback_timeout: Maximum time for a single callback invocation");
    println!("  - max_memory_bytes: Maximum WASM memory usage (enforced via ResourceLimiter)");
    println!("  - max_callback_invocations: Maximum number of callback calls");
    println!("\nDefault limits provide reasonable protection for most use cases.");

    Ok(())
}
