//! Example demonstrating runtime-defined callbacks using `DynamicCallback`.
//!
//! This example shows how to create callbacks dynamically at runtime,
//! where the callback name, description, schema, and behavior are all
//! determined at runtime rather than compile time.
//!
//! This is useful for:
//! - Plugin systems where callbacks are loaded from configuration
//! - Dynamic API wrappers where endpoints are discovered at runtime
//! - Proxying callbacks to external services
//!
//! Run with: `cargo run --example runtime_callbacks`

use std::collections::HashMap;

use eryx::{CallbackError, DynamicCallback, Sandbox};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Runtime-Defined Callbacks Example ===\n");
    println!("This example demonstrates creating callbacks dynamically at runtime,");
    println!("where names, schemas, and behavior are all determined at runtime.\n");

    // Create a "greet" callback dynamically
    let greet = DynamicCallback::builder("greet", "Greets a person by name", |args| {
        Box::pin(async move {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'name'".into()))?;

            let formal = args
                .get("formal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let greeting = if formal {
                format!("Good day, {name}. How may I assist you?")
            } else {
                format!("Hey {name}! What's up?")
            };

            Ok(json!({ "greeting": greeting }))
        })
    })
    .param("name", "string", "The person's name", true)
    .param("formal", "boolean", "Use formal greeting", false)
    .build();

    // Create a "calculate" callback dynamically
    let calculate = DynamicCallback::builder("calculate", "Performs a calculation", |args| {
        Box::pin(async move {
            let op = args
                .get("operation")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'operation'".into()))?;

            let a = args
                .get("a")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'a'".into()))?;

            let b = args
                .get("b")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'b'".into()))?;

            let result = match op {
                "add" => a + b,
                "sub" => a - b,
                "mul" => a * b,
                "div" => {
                    if b == 0.0 {
                        return Err(CallbackError::ExecutionFailed("division by zero".into()));
                    }
                    a / b
                }
                _ => {
                    return Err(CallbackError::InvalidArguments(format!(
                        "unknown operation: {op}"
                    )));
                }
            };

            Ok(json!({ "result": result, "operation": op }))
        })
    })
    .param(
        "operation",
        "string",
        "The operation: add, sub, mul, div",
        true,
    )
    .param("a", "number", "First operand", true)
    .param("b", "number", "Second operand", true)
    .build();

    // Create a "lookup" callback that simulates looking up data
    // This demonstrates a callback with captured state
    let data: HashMap<&str, Value> = [
        ("version", json!("1.0.0")),
        ("author", json!("Eryx Team")),
        ("features", json!(["sandbox", "callbacks", "async"])),
        ("config", json!({"debug": false, "timeout": 30})),
    ]
    .into_iter()
    .collect();

    let lookup = DynamicCallback::builder(
        "lookup",
        "Looks up a value from a predefined dictionary",
        move |args| {
            let data = data.clone();
            Box::pin(async move {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| CallbackError::InvalidArguments("missing 'key'".into()))?;

                let value = data.get(key).cloned().unwrap_or(Value::Null);
                Ok(json!({ "key": key, "value": value, "found": !value.is_null() }))
            })
        },
    )
    .param("key", "string", "The key to look up", true)
    .build();

    println!("Created 3 dynamic callbacks: greet, calculate, lookup\n");

    // Build the sandbox with dynamically created callbacks using embedded runtime
    let sandbox = Sandbox::embedded()
        .with_callback(greet)
        .with_callback(calculate)
        .with_callback(lookup)
        .build()?;

    println!(
        "Sandbox created with {} runtime-defined callbacks\n",
        sandbox.callbacks().len()
    );

    // Example 1: Using the dynamic "greet" callback
    println!("=== Example 1: Dynamic 'greet' callback ===");
    let result = sandbox
        .execute(
            r#"
# Use the dynamically-defined greet callback
informal = await greet(name="Alice")
print(f"Informal: {informal['greeting']}")

formal = await greet(name="Dr. Smith", formal=True)
print(f"Formal: {formal['greeting']}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);

    // Example 2: Using the dynamic "calculate" callback
    println!("=== Example 2: Dynamic 'calculate' callback ===");
    let result = sandbox
        .execute(
            r#"
# Perform various calculations using the dynamic callback
operations = [
    ("add", 10, 5),
    ("sub", 10, 5),
    ("mul", 10, 5),
    ("div", 10, 5),
]

for op, a, b in operations:
    result = await calculate(operation=op, a=a, b=b)
    print(f"{a} {op} {b} = {result['result']}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);

    // Example 3: Using the dynamic "lookup" callback
    println!("=== Example 3: Dynamic 'lookup' callback ===");
    let result = sandbox
        .execute(
            r#"
# Look up various keys
keys = ["version", "author", "features", "nonexistent"]

for key in keys:
    result = await lookup(key=key)
    if result['found']:
        print(f"{key}: {result['value']}")
    else:
        print(f"{key}: NOT FOUND")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);

    // Example 4: Introspection - Python can discover the dynamic callbacks
    println!("=== Example 4: Introspection ===");
    let result = sandbox
        .execute(
            r#"
# List all available callbacks (including our dynamic ones)
callbacks = list_callbacks()
print(f"Available callbacks ({len(callbacks)}):")
for cb in callbacks:
    print(f"  - {cb['name']}: {cb['description']}")
    # The schema is also available for introspection
    schema = cb.get('parameters_schema', {})
    if schema and 'properties' in schema:
        props = schema['properties']
        if props:
            print(f"    Parameters: {list(props.keys())}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);

    // Example 5: Error handling with dynamic callbacks
    println!("=== Example 5: Error handling ===");
    let result = sandbox
        .execute(
            r#"
# Test error handling with dynamic callbacks
try:
    result = await calculate(operation="div", a=10, b=0)
except Exception as e:
    print(f"Division by zero caught: {e}")

try:
    result = await calculate(operation="invalid_op", a=1, b=2)
except Exception as e:
    print(f"Invalid operation caught: {e}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);

    println!("\n=== Summary ===");
    println!("DynamicCallback is useful when:");
    println!("  - Callback definitions come from configuration files");
    println!("  - You're building a plugin system");
    println!("  - API endpoints are discovered dynamically");
    println!("  - You need to proxy calls to external services");
    println!("\nFor compile-time type safety, prefer TypedCallback.");
    println!("For maximum flexibility, use DynamicCallback or the base Callback trait.");

    Ok(())
}
