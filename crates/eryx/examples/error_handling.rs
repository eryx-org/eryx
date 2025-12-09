//! Example demonstrating error handling in the sandbox.
//!
//! This example shows how errors from Python code and callbacks
//! are propagated back to the Rust host.
//!
//! Run with: `cargo run --example error_handling`

use std::future::Future;
use std::pin::Pin;

use eryx::{Callback, CallbackError, Sandbox};
use serde_json::{Value, json};

/// A callback that always fails.
struct FailingCallback;

impl Callback for FailingCallback {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "A callback that always returns an error"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Error message to return"
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
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");

            Err(CallbackError::ExecutionFailed(message.to_string()))
        })
    }
}

/// A callback that validates its arguments.
struct ValidatingCallback;

impl Callback for ValidatingCallback {
    fn name(&self) -> &str {
        "validate"
    }

    fn description(&self) -> &str {
        "A callback that validates its arguments strictly"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "A value between 0 and 100"
                }
            },
            "required": ["value"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let value = args
                .get("value")
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'value' field".into()))?
                .as_i64()
                .ok_or_else(|| {
                    CallbackError::InvalidArguments("'value' must be an integer".into())
                })?;

            if value < 0 {
                return Err(CallbackError::InvalidArguments(
                    "value must be non-negative".into(),
                ));
            }

            if value > 100 {
                return Err(CallbackError::InvalidArguments(
                    "value must not exceed 100".into(),
                ));
            }

            Ok(json!({ "validated": value }))
        })
    }
}

async fn run_python_error_examples(sandbox: &Sandbox) -> anyhow::Result<()> {
    // Example 1: Python syntax error
    println!("=== Example 1: Python syntax error ===");
    let result = sandbox
        .execute(
            "
def broken(
    # Missing closing parenthesis and colon
print('never reached')
",
        )
        .await;

    match result {
        Ok(_) => println!("Unexpected success!"),
        Err(e) => println!("Error (expected): {e}"),
    }
    println!();

    // Example 2: Python runtime error
    println!("=== Example 2: Python runtime error (undefined variable) ===");
    let result = sandbox
        .execute(
            "
x = 10
y = x + undefined_variable
",
        )
        .await;

    match result {
        Ok(_) => println!("Unexpected success!"),
        Err(e) => println!("Error (expected): {e}"),
    }
    println!();

    // Example 3: Python division by zero
    println!("=== Example 3: Python division by zero ===");
    let result = sandbox
        .execute(
            "
result = 42 / 0
",
        )
        .await;

    match result {
        Ok(_) => println!("Unexpected success!"),
        Err(e) => println!("Error (expected): {e}"),
    }
    println!();

    // Example 7: Type error in Python
    println!("=== Example 7: Python type error ===");
    let result = sandbox
        .execute(
            "
x = 'hello'
y = x + 42  # Can't concatenate str and int
",
        )
        .await;

    match result {
        Ok(_) => println!("Unexpected success!"),
        Err(e) => println!("Error (expected): {e}"),
    }
    println!();

    Ok(())
}

async fn run_callback_error_examples(sandbox: &Sandbox) -> anyhow::Result<()> {
    // Example 4: Callback error
    println!("=== Example 4: Callback that returns an error ===");
    let result = sandbox
        .execute(
            r#"
try:
    result = await fail(message="Something went wrong!")
    print(f"Result: {result}")
except Exception as e:
    print(f"Caught exception in Python: {e}")
"#,
        )
        .await;

    match result {
        Ok(r) => println!("Output: {}", r.stdout),
        Err(e) => println!("Error: {e}"),
    }
    println!();

    // Example 5: Callback with invalid arguments
    println!("=== Example 5: Callback argument validation ===");
    let result = sandbox
        .execute(
            r#"
# Try valid value
result = await validate(value=50)
print(f"Valid (50): {result}")

# Try boundary values
result = await validate(value=0)
print(f"Valid (0): {result}")

result = await validate(value=100)
print(f"Valid (100): {result}")

# Try invalid value (will raise exception)
try:
    result = await validate(value=150)
    print(f"Invalid (150): {result}")
except Exception as e:
    print(f"Validation error for 150: {e}")

# Try missing argument
try:
    result = await validate()
    print(f"Missing arg: {result}")
except Exception as e:
    print(f"Validation error for missing arg: {e}")
"#,
        )
        .await;

    match result {
        Ok(r) => {
            println!("Output:");
            for line in r.stdout.lines() {
                println!("  {line}");
            }
        }
        Err(e) => println!("Error: {e}"),
    }
    println!();

    // Example 6: Calling non-existent callback
    println!("=== Example 6: Non-existent callback ===");
    let result = sandbox
        .execute(
            r#"
try:
    # Use invoke() for dynamic/unknown callback names
    result = await invoke("nonexistent_callback")
    print(f"Result: {result}")
except Exception as e:
    print(f"Error calling nonexistent callback: {e}")
"#,
        )
        .await;

    match result {
        Ok(r) => println!("Output: {}", r.stdout),
        Err(e) => println!("Error: {e}"),
    }
    println!();

    Ok(())
}

async fn run_graceful_error_handling_example(sandbox: &Sandbox) -> anyhow::Result<()> {
    // Example 8: Graceful error handling in Python
    println!("=== Example 8: Graceful error handling pattern ===");
    let result = sandbox
        .execute(
            r#"
async def safe_invoke(name, **kwargs):
    """Wrapper that catches callback errors and returns None."""
    try:
        return await invoke(name, **kwargs)
    except Exception as e:
        print(f"Warning: {name} failed with {e}")
        return None

# This pattern allows code to continue even if some callbacks fail
results = []
for i, value in enumerate([50, 150, 75, -10, 25]):
    result = await safe_invoke("validate", value=value)
    if result is not None:
        results.append(result)
        print(f"  Value {value}: OK")
    else:
        print(f"  Value {value}: FAILED")

print(f"\nSuccessfully validated {len(results)} out of 5 values")
"#,
        )
        .await;

    match result {
        Ok(r) => {
            println!("Output:");
            for line in r.stdout.lines() {
                println!("  {line}");
            }
        }
        Err(e) => println!("Error: {e}"),
    }
    println!();

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Path to the WASM runtime component
    let wasm_path = std::env::var("ERYX_WASM_PATH")
        .unwrap_or_else(|_| "crates/eryx-runtime/runtime.wasm".to_string());

    println!("Loading WASM component from: {wasm_path}");

    // Build the sandbox with our error-prone callbacks
    let sandbox = Sandbox::builder()
        .with_wasm_file(&wasm_path)
        .with_callback(FailingCallback)
        .with_callback(ValidatingCallback)
        .build()?;

    println!("Sandbox created");
    println!();

    run_python_error_examples(&sandbox).await?;
    run_callback_error_examples(&sandbox).await?;
    run_graceful_error_handling_example(&sandbox).await?;

    println!("Error handling examples completed!");

    Ok(())
}
