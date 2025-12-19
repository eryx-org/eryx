//! Simple example demonstrating basic Python execution in the sandbox.
//!
//! This example shows two ways to define callbacks:
//! - `GetTime`: Uses `TypedCallback` for strongly-typed, zero-argument callbacks
//! - `Echo`: Uses `TypedCallback` with a typed argument struct
//!
//! Run with: `cargo run --example simple`

use std::future::Future;
use std::pin::Pin;

use eryx::JsonSchema;
use eryx::{CallbackError, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

/// A simple callback that returns the current Unix timestamp.
///
/// This demonstrates `TypedCallback` with no arguments (using `()` as the Args type).
struct GetTime;

impl TypedCallback for GetTime {
    // Unit type for callbacks that take no arguments
    type Args = ();

    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Returns the current Unix timestamp in seconds"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .as_secs();
            Ok(json!(now))
        })
    }
}

/// Strongly-typed arguments for the Echo callback.
///
/// The `JsonSchema` derive automatically generates the JSON Schema
/// that will be exposed to Python and LLMs for introspection.
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// The message to echo back
    message: String,
}

/// A callback that echoes back the input arguments.
///
/// This demonstrates `TypedCallback` with a typed argument struct.
/// The schema is automatically generated from `EchoArgs`.
struct Echo;

impl TypedCallback for Echo {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the provided message"
    }

    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            // No manual JSON parsing needed - args is already typed!
            Ok(json!({ "echoed": args.message }))
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build the sandbox with callbacks using the embedded runtime
    // Both GetTime and Echo implement Callback via the TypedCallback blanket impl
    let sandbox = Sandbox::embedded()
        .with_callback(GetTime)
        .with_callback(Echo)
        .build()?;

    println!(
        "Sandbox created with {} callbacks",
        sandbox.callbacks().len()
    );
    println!();

    // Example 1: Simple Python code
    println!("=== Example 1: Simple Python code ===");
    let result = sandbox
        .execute(
            r#"
print("Hello from Python!")
print(f"2 + 2 = {2 + 2}")
"#,
        )
        .await?;

    println!("Output: {}", result.stdout);
    println!("Duration: {:?}", result.stats.duration);
    println!(
        "Peak memory: {} bytes",
        result.stats.peak_memory_bytes.unwrap_or(0)
    );
    println!();

    // Example 2: Using a callback
    println!("=== Example 2: Using a callback ===");
    let result = sandbox
        .execute(
            r#"
timestamp = await get_time()
print(f"Current Unix timestamp: {timestamp}")
"#,
        )
        .await?;

    println!("Output: {}", result.stdout);
    println!("Callbacks invoked: {}", result.stats.callback_invocations);
    println!(
        "Peak memory: {} bytes",
        result.stats.peak_memory_bytes.unwrap_or(0)
    );
    println!();

    // Example 3: Echo callback with arguments
    println!("=== Example 3: Echo callback with arguments ===");
    let result = sandbox
        .execute(
            r#"
response = await echo(message="Hello from the sandbox!")
print(f"Echo response: {response}")
"#,
        )
        .await?;

    println!("Output: {}", result.stdout);
    println!();

    // Example 4: List available callbacks
    println!("=== Example 4: Introspection - list callbacks ===");
    let result = sandbox
        .execute(
            r#"
callbacks = list_callbacks()
print(f"Available callbacks ({len(callbacks)}):")
for cb in callbacks:
    print(f"  - {cb['name']}: {cb['description']}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!();

    // Example 5: Using Python's asyncio.gather for parallel execution
    println!("=== Example 5: Parallel callback execution ===");
    let result = sandbox
        .execute(
            r#"
import asyncio

# Execute multiple callbacks in parallel
results = await asyncio.gather(
    echo(message="first"),
    echo(message="second"),
    echo(message="third"),
)

for i, result in enumerate(results):
    print(f"Result {i + 1}: {result}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!(
        "Total callbacks invoked: {}",
        result.stats.callback_invocations
    );
    println!();

    println!("All examples completed successfully!");

    Ok(())
}
