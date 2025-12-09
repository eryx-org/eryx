//! Example demonstrating the use of `RuntimeLibrary` for composable callbacks.
//!
//! `RuntimeLibrary` allows you to bundle together:
//! - Callbacks that Python code can invoke
//! - Python preamble code (helper classes, wrapper functions, etc.)
//! - Type stubs (.pyi content) for IDE support and LLM context
//!
//! This is useful for creating reusable integrations that can be shared
//! across multiple sandboxes or distributed as libraries.
//!
//! Run with: `cargo run --example custom_library`

use std::future::Future;
use std::pin::Pin;

use eryx::{Callback, CallbackError, RuntimeLibrary, Sandbox};
use serde_json::{Value, json};

// =============================================================================
// Math Library - A collection of mathematical operations
// =============================================================================

/// Callback that adds two numbers.
struct MathAdd;

impl Callback for MathAdd {
    fn name(&self) -> &str {
        "math.add"
    }

    fn description(&self) -> &str {
        "Add two numbers together"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number", "description": "First operand" },
                "b": { "type": "number", "description": "Second operand" }
            },
            "required": ["a", "b"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let a = args
                .get("a")
                .and_then(Value::as_f64)
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'a' field".into()))?;
            let b = args
                .get("b")
                .and_then(Value::as_f64)
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'b' field".into()))?;

            Ok(json!(a + b))
        })
    }
}

/// Callback that multiplies two numbers.
struct MathMultiply;

impl Callback for MathMultiply {
    fn name(&self) -> &str {
        "math.multiply"
    }

    fn description(&self) -> &str {
        "Multiply two numbers together"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number", "description": "First operand" },
                "b": { "type": "number", "description": "Second operand" }
            },
            "required": ["a", "b"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let a = args
                .get("a")
                .and_then(Value::as_f64)
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'a' field".into()))?;
            let b = args
                .get("b")
                .and_then(Value::as_f64)
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'b' field".into()))?;

            Ok(json!(a * b))
        })
    }
}

/// Callback that computes the power of a number.
struct MathPower;

impl Callback for MathPower {
    fn name(&self) -> &str {
        "math.power"
    }

    fn description(&self) -> &str {
        "Raise a number to a power"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "base": { "type": "number", "description": "The base number" },
                "exponent": { "type": "number", "description": "The exponent" }
            },
            "required": ["base", "exponent"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let base = args
                .get("base")
                .and_then(Value::as_f64)
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'base' field".into()))?;
            let exponent = args
                .get("exponent")
                .and_then(Value::as_f64)
                .ok_or_else(|| {
                    CallbackError::InvalidArguments("missing 'exponent' field".into())
                })?;

            Ok(json!(base.powf(exponent)))
        })
    }
}

/// Create a math library with callbacks and a Python wrapper class.
fn create_math_library() -> RuntimeLibrary {
    // Python preamble that provides a nice wrapper class
    let preamble = r#"
class Math:
    """A helper class for mathematical operations.

    Provides a cleaner API than calling invoke() directly.

    Example:
        math = Math()
        result = await math.add(2, 3)  # Returns 5.0
        result = await math.multiply(4, 5)  # Returns 20.0
        result = await math.power(2, 10)  # Returns 1024.0
    """

    async def add(self, a: float, b: float) -> float:
        """Add two numbers together."""
        result = await invoke("math.add", json.dumps({"a": a, "b": b}))
        return result

    async def multiply(self, a: float, b: float) -> float:
        """Multiply two numbers together."""
        result = await invoke("math.multiply", json.dumps({"a": a, "b": b}))
        return result

    async def power(self, base: float, exponent: float) -> float:
        """Raise a number to a power."""
        result = await invoke("math.power", json.dumps({"base": base, "exponent": exponent}))
        return result
"#;

    // Type stubs for IDE support and LLM context
    let stubs = r#"
class Math:
    """A helper class for mathematical operations."""

    async def add(self, a: float, b: float) -> float:
        """Add two numbers together."""
        ...

    async def multiply(self, a: float, b: float) -> float:
        """Multiply two numbers together."""
        ...

    async def power(self, base: float, exponent: float) -> float:
        """Raise a number to a power."""
        ...
"#;

    RuntimeLibrary::new()
        .with_callback(MathAdd)
        .with_callback(MathMultiply)
        .with_callback(MathPower)
        .with_preamble(preamble)
        .with_stubs(stubs)
}

// =============================================================================
// Storage Library - Simple key-value storage
// =============================================================================

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared storage state for the key-value store.
type Storage = Arc<Mutex<HashMap<String, Value>>>;

/// Callback that stores a value.
struct StorageSet {
    storage: Storage,
}

impl Callback for StorageSet {
    fn name(&self) -> &str {
        "storage.set"
    }

    fn description(&self) -> &str {
        "Store a value with the given key"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The storage key" },
                "value": { "description": "The value to store (any JSON value)" }
            },
            "required": ["key", "value"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let storage = self.storage.clone();
        Box::pin(async move {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'key' field".into()))?
                .to_string();
            let value = args
                .get("value")
                .cloned()
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'value' field".into()))?;

            storage
                .lock()
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .insert(key, value);

            Ok(json!({"success": true}))
        })
    }
}

/// Callback that retrieves a value.
struct StorageGet {
    storage: Storage,
}

impl Callback for StorageGet {
    fn name(&self) -> &str {
        "storage.get"
    }

    fn description(&self) -> &str {
        "Retrieve a value by key"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "The storage key" }
            },
            "required": ["key"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let storage = self.storage.clone();
        Box::pin(async move {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CallbackError::InvalidArguments("missing 'key' field".into()))?;

            let value = storage
                .lock()
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .get(key)
                .cloned();

            Ok(value.unwrap_or(Value::Null))
        })
    }
}

/// Callback that lists all keys.
struct StorageKeys {
    storage: Storage,
}

impl Callback for StorageKeys {
    fn name(&self) -> &str {
        "storage.keys"
    }

    fn description(&self) -> &str {
        "List all storage keys"
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
        let storage = self.storage.clone();
        Box::pin(async move {
            let keys: Vec<String> = storage
                .lock()
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .keys()
                .cloned()
                .collect();

            Ok(json!(keys))
        })
    }
}

/// Create a storage library with shared state.
fn create_storage_library() -> RuntimeLibrary {
    let storage: Storage = Arc::new(Mutex::new(HashMap::new()));

    let preamble = r#"
class Storage:
    """A simple key-value storage interface.

    Example:
        storage = Storage()
        await storage.set("user", {"name": "Alice", "age": 30})
        user = await storage.get("user")
        keys = await storage.keys()
    """

    async def set(self, key: str, value) -> bool:
        """Store a value with the given key."""
        result = await invoke("storage.set", json.dumps({"key": key, "value": value}))
        return result.get("success", False)

    async def get(self, key: str):
        """Retrieve a value by key. Returns None if not found."""
        return await invoke("storage.get", json.dumps({"key": key}))

    async def keys(self) -> list:
        """List all storage keys."""
        return await invoke("storage.keys", "{}")
"#;

    let stubs = r#"
from typing import Any, List, Optional

class Storage:
    """A simple key-value storage interface."""

    async def set(self, key: str, value: Any) -> bool:
        """Store a value with the given key."""
        ...

    async def get(self, key: str) -> Optional[Any]:
        """Retrieve a value by key. Returns None if not found."""
        ...

    async def keys(self) -> List[str]:
        """List all storage keys."""
        ...
"#;

    RuntimeLibrary::new()
        .with_callback(StorageSet {
            storage: storage.clone(),
        })
        .with_callback(StorageGet {
            storage: storage.clone(),
        })
        .with_callback(StorageKeys { storage })
        .with_preamble(preamble)
        .with_stubs(stubs)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Path to the WASM runtime component
    let wasm_path = std::env::var("ERYX_WASM_PATH")
        .unwrap_or_else(|_| "crates/eryx-runtime/runtime.wasm".to_string());

    println!("=== RuntimeLibrary Example ===\n");
    println!("Loading WASM component from: {wasm_path}\n");

    // Create our libraries
    let math_lib = create_math_library();
    let storage_lib = create_storage_library();

    // Display the type stubs (useful for LLM context)
    println!("=== Combined Type Stubs ===");
    let combined_stubs = format!("{}\n{}", math_lib.type_stubs, storage_lib.type_stubs);
    println!("{combined_stubs}");
    println!();

    // Build sandbox with both libraries merged
    let sandbox = Sandbox::builder()
        .with_wasm_file(&wasm_path)
        .with_library(math_lib)
        .with_library(storage_lib)
        .build()?;

    println!(
        "Sandbox created with {} callbacks\n",
        sandbox.callbacks().len()
    );

    // Example 1: Using the Math library wrapper class
    println!("=== Example 1: Math Library ===");
    let result = sandbox
        .execute(
            r#"
math = Math()

# Basic operations
sum_result = await math.add(10, 20)
print(f"10 + 20 = {sum_result}")

product = await math.multiply(7, 8)
print(f"7 * 8 = {product}")

power = await math.power(2, 10)
print(f"2^10 = {power}")

# Chaining operations (calculate (3 + 4) * 5)
step1 = await math.add(3, 4)
step2 = await math.multiply(step1, 5)
print(f"(3 + 4) * 5 = {step2}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!("Callbacks invoked: {}\n", result.stats.callback_invocations);

    // Example 2: Using the Storage library wrapper class
    println!("=== Example 2: Storage Library ===");
    let result = sandbox
        .execute(
            r#"
storage = Storage()

# Store some data
await storage.set("user", {"name": "Alice", "role": "admin"})
await storage.set("config", {"theme": "dark", "language": "en"})
await storage.set("counter", 42)

# Retrieve and display
user = await storage.get("user")
print(f"User: {user}")

config = await storage.get("config")
print(f"Config: {config}")

counter = await storage.get("counter")
print(f"Counter: {counter}")

# List all keys
keys = await storage.keys()
print(f"All keys: {keys}")

# Try to get a non-existent key
missing = await storage.get("nonexistent")
print(f"Missing key returns: {missing}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!("Callbacks invoked: {}\n", result.stats.callback_invocations);

    // Example 3: Using both libraries together
    println!("=== Example 3: Combined Usage ===");
    let result = sandbox
        .execute(
            r#"
math = Math()
storage = Storage()

# Store some numbers
await storage.set("a", 15)
await storage.set("b", 7)

# Retrieve and compute
a = await storage.get("a")
b = await storage.get("b")

sum_ab = await math.add(a, b)
product_ab = await math.multiply(a, b)

print(f"Retrieved a={a}, b={b}")
print(f"a + b = {sum_ab}")
print(f"a * b = {product_ab}")

# Store the results
await storage.set("sum", sum_ab)
await storage.set("product", product_ab)

# Show all stored data
keys = await storage.keys()
print(f"\nAll stored keys: {keys}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!("Callbacks invoked: {}\n", result.stats.callback_invocations);

    // Example 4: Parallel operations with both libraries
    println!("=== Example 4: Parallel Operations ===");
    let result = sandbox
        .execute(
            r#"
import asyncio

math = Math()

# Run multiple math operations in parallel
results = await asyncio.gather(
    math.add(1, 2),
    math.multiply(3, 4),
    math.power(2, 8),
    math.add(100, 200),
)

print(f"Parallel results: {results}")
print(f"  1 + 2 = {results[0]}")
print(f"  3 * 4 = {results[1]}")
print(f"  2^8 = {results[2]}")
print(f"  100 + 200 = {results[3]}")
"#,
        )
        .await?;

    println!("Output:\n{}", result.stdout);
    println!(
        "Callbacks invoked: {} (executed in parallel!)\n",
        result.stats.callback_invocations
    );

    println!("=== All examples completed successfully! ===");

    Ok(())
}
