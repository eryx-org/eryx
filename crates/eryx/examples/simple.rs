//! Simple example demonstrating basic Python execution in the sandbox.
//!
//! Run with: `cargo run --example simple`

use std::future::Future;
use std::pin::Pin;

use eryx::{Callback, CallbackError, Sandbox};
use serde_json::{Value, json};

/// A simple callback that returns the current Unix timestamp.
struct GetTime;

impl Callback for GetTime {
    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Returns the current Unix timestamp in seconds"
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
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .as_secs();
            Ok(json!(now))
        })
    }
}

/// A callback that echoes back the input arguments.
struct Echo;

impl Callback for Echo {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the provided message"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to echo"
                }
            },
            "required": ["message"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'message' field".into()))?;

            Ok(json!({ "echoed": message }))
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Path to the WASM runtime component
    let wasm_path = std::env::var("ERYX_WASM_PATH")
        .unwrap_or_else(|_| "crates/eryx-runtime/runtime.wasm".to_string());

    println!("Loading WASM component from: {wasm_path}");

    // Build the sandbox with callbacks
    let sandbox = Sandbox::builder()
        .with_wasm_file(&wasm_path)
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
    println!();

    // Example 2: Using a callback
    println!("=== Example 2: Using a callback ===");
    let result = sandbox
        .execute(
            r#"
timestamp = await invoke("get_time", "{}")
print(f"Current Unix timestamp: {timestamp}")
"#,
        )
        .await?;

    println!("Output: {}", result.stdout);
    println!("Callbacks invoked: {}", result.stats.callback_invocations);
    println!();

    // Example 3: Echo callback with arguments
    println!("=== Example 3: Echo callback with arguments ===");
    let result = sandbox
        .execute(
            r#"
response = await invoke("echo", '{"message": "Hello from the sandbox!"}')
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
    invoke("echo", '{"message": "first"}'),
    invoke("echo", '{"message": "second"}'),
    invoke("echo", '{"message": "third"}'),
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
