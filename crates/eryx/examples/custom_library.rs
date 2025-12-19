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

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use eryx::JsonSchema;
use eryx::{CallbackError, RuntimeLibrary, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Math Library - A collection of mathematical operations
// =============================================================================

/// Arguments for binary math operations (add, multiply).
#[derive(Deserialize, JsonSchema)]
struct BinaryMathArgs {
    /// First operand
    a: f64,
    /// Second operand
    b: f64,
}

/// Arguments for power operation.
#[derive(Deserialize, JsonSchema)]
struct PowerArgs {
    /// The base number
    base: f64,
    /// The exponent
    exponent: f64,
}

/// Callback that adds two numbers.
struct MathAdd;

impl TypedCallback for MathAdd {
    type Args = BinaryMathArgs;

    fn name(&self) -> &str {
        "math.add"
    }

    fn description(&self) -> &str {
        "Add two numbers together"
    }

    fn invoke_typed(
        &self,
        args: BinaryMathArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!(args.a + args.b)) })
    }
}

/// Callback that multiplies two numbers.
struct MathMultiply;

impl TypedCallback for MathMultiply {
    type Args = BinaryMathArgs;

    fn name(&self) -> &str {
        "math.multiply"
    }

    fn description(&self) -> &str {
        "Multiply two numbers together"
    }

    fn invoke_typed(
        &self,
        args: BinaryMathArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!(args.a * args.b)) })
    }
}

/// Callback that computes the power of a number.
struct MathPower;

impl TypedCallback for MathPower {
    type Args = PowerArgs;

    fn name(&self) -> &str {
        "math.power"
    }

    fn description(&self) -> &str {
        "Raise a number to a power"
    }

    fn invoke_typed(
        &self,
        args: PowerArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!(args.base.powf(args.exponent))) })
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
        return await invoke("math.add", a=a, b=b)

    async def multiply(self, a: float, b: float) -> float:
        """Multiply two numbers together."""
        return await invoke("math.multiply", a=a, b=b)

    async def power(self, base: float, exponent: float) -> float:
        """Raise a number to a power."""
        return await invoke("math.power", base=base, exponent=exponent)
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

/// Shared storage state for the key-value store.
type Storage = Arc<Mutex<HashMap<String, Value>>>;

/// Arguments for storage.set operation.
#[derive(Deserialize, JsonSchema)]
struct StorageSetArgs {
    /// The storage key
    key: String,
    /// The value to store (any JSON value)
    value: Value,
}

/// Arguments for storage.get operation.
#[derive(Deserialize, JsonSchema)]
struct StorageGetArgs {
    /// The storage key
    key: String,
}

/// Callback that stores a value.
struct StorageSet {
    storage: Storage,
}

impl TypedCallback for StorageSet {
    type Args = StorageSetArgs;

    fn name(&self) -> &str {
        "storage.set"
    }

    fn description(&self) -> &str {
        "Store a value with the given key"
    }

    fn invoke_typed(
        &self,
        args: StorageSetArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let storage = self.storage.clone();
        Box::pin(async move {
            storage
                .lock()
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .insert(args.key, args.value);

            Ok(json!({"success": true}))
        })
    }
}

/// Callback that retrieves a value.
struct StorageGet {
    storage: Storage,
}

impl TypedCallback for StorageGet {
    type Args = StorageGetArgs;

    fn name(&self) -> &str {
        "storage.get"
    }

    fn description(&self) -> &str {
        "Retrieve a value by key"
    }

    fn invoke_typed(
        &self,
        args: StorageGetArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let storage = self.storage.clone();
        Box::pin(async move {
            let value = storage
                .lock()
                .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?
                .get(&args.key)
                .cloned();

            Ok(value.unwrap_or(Value::Null))
        })
    }
}

/// Callback that lists all keys.
struct StorageKeys {
    storage: Storage,
}

impl TypedCallback for StorageKeys {
    type Args = ();

    fn name(&self) -> &str {
        "storage.keys"
    }

    fn description(&self) -> &str {
        "List all storage keys"
    }

    fn invoke_typed(
        &self,
        _args: (),
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
        result = await invoke("storage.set", key=key, value=value)
        return result.get("success", False)

    async def get(self, key: str):
        """Retrieve a value by key. Returns None if not found."""
        return await invoke("storage.get", key=key)

    async def keys(self) -> list:
        """List all storage keys."""
        return await invoke("storage.keys")
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
    println!("=== RuntimeLibrary Example ===\n");

    // Create our libraries
    let math_lib = create_math_library();
    let storage_lib = create_storage_library();

    // Display the type stubs (useful for LLM context)
    println!("=== Combined Type Stubs ===");
    let combined_stubs = format!("{}\n{}", math_lib.type_stubs, storage_lib.type_stubs);
    println!("{combined_stubs}");
    println!();

    // Build sandbox with both libraries merged using embedded runtime
    let sandbox = Sandbox::embedded()
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
