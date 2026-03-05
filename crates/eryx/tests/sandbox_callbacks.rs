//! Integration tests for Sandbox callback handling.
//!
//! These tests verify that callbacks work correctly through the high-level
//! Sandbox API, including error handling, validation, and various edge cases.
//!
//! Unlike the SessionExecutor tests, these tests use the full Sandbox which
//! handles callback channel setup automatically.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::approx_constant)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use eryx::{CallbackError, JsonSchema, ResourceLimits, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Test Callbacks
// =============================================================================

/// A callback that always succeeds and returns a simple response.
struct SucceedCallback;

impl TypedCallback for SucceedCallback {
    type Args = ();

    fn name(&self) -> &str {
        "succeed"
    }

    fn description(&self) -> &str {
        "Always succeeds with a simple response"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"status": "ok"})) })
    }
}

/// Arguments for the echo callback.
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// The message to echo back
    message: String,
}

/// A callback that echoes back the message.
struct EchoCallback;

impl TypedCallback for EchoCallback {
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
        Box::pin(async move { Ok(json!({"echoed": args.message})) })
    }
}

/// Arguments for the failing callback.
#[derive(Deserialize, JsonSchema)]
struct FailArgs {
    /// Error message to return
    message: String,
}

/// A callback that always fails with the provided message.
struct FailingCallback;

impl TypedCallback for FailingCallback {
    type Args = FailArgs;

    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails with the provided error message"
    }

    fn invoke_typed(
        &self,
        args: FailArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Err(CallbackError::ExecutionFailed(args.message)) })
    }
}

/// Arguments for the validating callback.
#[derive(Deserialize, JsonSchema)]
struct ValidateArgs {
    /// Value to validate (must be 0-100)
    value: i64,
}

/// A callback that validates its input range.
struct ValidatingCallback;

impl TypedCallback for ValidatingCallback {
    type Args = ValidateArgs;

    fn name(&self) -> &str {
        "validate"
    }

    fn description(&self) -> &str {
        "Validates that value is between 0 and 100"
    }

    fn invoke_typed(
        &self,
        args: ValidateArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            if args.value < 0 {
                return Err(CallbackError::InvalidArguments(
                    "value must be >= 0".to_string(),
                ));
            }
            if args.value > 100 {
                return Err(CallbackError::InvalidArguments(
                    "value must be <= 100".to_string(),
                ));
            }
            Ok(json!({"validated": args.value}))
        })
    }
}

/// Arguments for the add callback.
#[derive(Deserialize, JsonSchema)]
struct AddArgs {
    /// First number
    a: i64,
    /// Second number
    b: i64,
}

/// A callback that adds two numbers.
struct AddCallback;

impl TypedCallback for AddCallback {
    type Args = AddArgs;

    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> &str {
        "Adds two numbers together"
    }

    fn invoke_typed(
        &self,
        args: AddArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"result": args.a + args.b})) })
    }
}

/// Arguments for the search dashboards callback.
#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Search query string
    query: String,
}

/// A callback with a dotted name for namespace testing.
struct SearchDashboardsCallback;

impl TypedCallback for SearchDashboardsCallback {
    type Args = SearchArgs;

    fn name(&self) -> &str {
        "search.dashboards"
    }

    fn description(&self) -> &str {
        "Search for dashboards by query"
    }

    fn invoke_typed(
        &self,
        args: SearchArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"dashboards": [{"name": args.query}]})) })
    }
}

/// Arguments for the sleep callback.
#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Milliseconds to sleep
    ms: u64,
}

/// A callback that sleeps for the specified duration.
struct SleepCallback;

impl TypedCallback for SleepCallback {
    type Args = SleepArgs;

    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleeps for the specified milliseconds"
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

/// Arguments for the list_items callback (all optional, like list_datasources).
#[derive(Deserialize, JsonSchema)]
struct ListItemsArgs {
    /// Optional name filter
    name: Option<String>,
    /// Optional type filters
    types: Option<Vec<String>>,
}

/// A callback with all-optional parameters, simulating list_datasources.
/// Can be called with zero arguments.
struct ListItemsCallback;

impl TypedCallback for ListItemsCallback {
    type Args = ListItemsArgs;

    fn name(&self) -> &str {
        "list_items"
    }

    fn description(&self) -> &str {
        "Lists items with optional filters"
    }

    fn invoke_typed(
        &self,
        args: ListItemsArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            // Return a response with special characters to exercise
            // the result serialization path
            let items = vec![
                json!({"name": "item1", "type": "prometheus", "url": "http://prom:9090"}),
                json!({"name": "item2", "type": "loki", "url": "http://loki:3100"}),
                json!({"name": "item'''3", "type": "tempo", "url": "C:\\path\\to\\thing"}),
            ];

            let filtered: Vec<_> = items
                .into_iter()
                .filter(|item| {
                    if let Some(ref name) = args.name
                        && item["name"].as_str() != Some(name.as_str())
                    {
                        return false;
                    }
                    if let Some(ref types) = args.types
                        && let Some(t) = item["type"].as_str()
                        && !types.iter().any(|ft| ft == t)
                    {
                        return false;
                    }
                    true
                })
                .collect();

            Ok(json!({"items": filtered, "count": filtered.len()}))
        })
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> PathBuf {
    // Check ERYX_PYTHON_STDLIB env var first (used in CI)
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    // Fall back to relative path from crate directory
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

/// Create a sandbox builder with the appropriate WASM source.
fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    // When embedded feature is available, use it (more reliable)
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }

    // Fallback to explicit paths for testing without embedded feature
    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

// =============================================================================
// Basic Callback Tests
// =============================================================================

#[tokio::test]
async fn test_callback_simple_success() {
    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await succeed()
print(f"Result: {result}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should succeed: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("ok"),
        "Should contain 'ok': {}",
        output.stdout
    );
    assert_eq!(output.stats.callback_invocations, 1);
}

#[tokio::test]
async fn test_callback_with_arguments() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await echo(message="Hello, World!")
print(f"Echoed: {result['echoed']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should succeed: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Hello, World!"),
        "Should echo the message: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_unicode_arguments() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await echo(message="Hello 世界 🌍 مرحبا")
print(f"Echoed: {result['echoed']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Unicode should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("世界") || output.stdout.contains("echoed"),
        "Should handle unicode: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_multiple_callback_invocations() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
for i in range(5):
    result = await echo(message=f"Message {i}")
    print(f"Got: {result['echoed']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should succeed: {:?}", result);
    let output = result.unwrap();
    assert_eq!(output.stats.callback_invocations, 5);
    assert!(output.stdout.contains("Message 4"));
}

// =============================================================================
// Callback Error Handling Tests
// =============================================================================

#[tokio::test]
async fn test_callback_error_caught_in_python() {
    let sandbox = sandbox_builder()
        .with_callback(FailingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await fail(message="intentional error")
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Python should catch the exception: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CAUGHT"),
        "Should have caught exception: {}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("UNEXPECTED SUCCESS"),
        "Should not have succeeded"
    );
}

#[tokio::test]
async fn test_callback_error_uncaught_propagates() {
    let sandbox = sandbox_builder()
        .with_callback(FailingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await fail(message="uncaught error")
print("never reached")
"#,
        )
        .await;

    assert!(result.is_err(), "Uncaught callback error should fail");
    let error = format!("{}", result.unwrap_err());
    assert!(
        error.contains("uncaught error") || error.contains("fail"),
        "Error should mention the callback: {}",
        error
    );
}

#[tokio::test]
async fn test_callback_validation_error_out_of_range() {
    let sandbox = sandbox_builder()
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await validate(value=150)
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Python should catch validation error: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CAUGHT"),
        "Should catch validation error: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_validation_error_negative() {
    let sandbox = sandbox_builder()
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await validate(value=-10)
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Python should catch validation error: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CAUGHT"),
        "Should catch validation error: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_missing_required_argument() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await echo()  # Missing required 'message' argument
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Python should catch missing arg: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CAUGHT"),
        "Should catch missing argument: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_wrong_argument_type() {
    let sandbox = sandbox_builder()
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await validate(value="not_a_number")
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Python should catch type error: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CAUGHT"),
        "Should catch type error: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_success_after_caught_error() {
    let sandbox = sandbox_builder()
        .with_callback(FailingCallback)
        .with_callback(SucceedCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# First, a callback that fails (caught)
try:
    await fail(message="first error")
except:
    print("caught first error")

# Then a callback that succeeds
result = await succeed()
print(f"Second result: {result}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Should succeed after caught error: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(output.stdout.contains("caught first error"));
    assert!(output.stdout.contains("ok"));
    assert_eq!(output.stats.callback_invocations, 2);
}

// =============================================================================
// Callback Introspection Tests
// =============================================================================

#[tokio::test]
async fn test_list_callbacks() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .with_callback(SucceedCallback)
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
callbacks = list_callbacks()
print(f"Found {len(callbacks)} callbacks")
for cb in callbacks:
    print(f"  - {cb['name']}: {cb['description']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should list callbacks: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("echo"));
    assert!(output.stdout.contains("succeed"));
    assert!(output.stdout.contains("validate"));
}

#[tokio::test]
async fn test_list_callbacks_includes_schema() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import json
callbacks = list_callbacks()
for cb in callbacks:
    schema = cb.get('parameters_schema')
    if schema:
        print(f"{cb['name']}: {json.dumps(schema)}")
    else:
        print(f"{cb['name']}: no schema")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Should list callbacks with schema: {:?}",
        result
    );
    let output = result.unwrap();
    // EchoCallback has a 'message' parameter - schema should include it
    assert!(
        output.stdout.contains("message"),
        "Echo callback schema should contain 'message' field: {}",
        output.stdout
    );
    // ValidatingCallback has a 'value' parameter - schema should include it
    assert!(
        output.stdout.contains("value"),
        "Validate callback schema should contain 'value' field: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_invoke_by_name() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# Use invoke() to call callback by name dynamically
result = await invoke("echo", message="dynamic call")
print(f"Result: {result}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should invoke by name: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("dynamic call"),
        "Should echo message: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_invoke_nonexistent_callback() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    result = await invoke("nonexistent_callback")
    print("UNEXPECTED SUCCESS")
except Exception as e:
    print(f"CAUGHT: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should catch not found: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("CAUGHT"));
}

// =============================================================================
// Resource Limits Tests
// =============================================================================

#[tokio::test]
async fn test_callback_invocation_limit() {
    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_resource_limits(ResourceLimits {
            max_callback_invocations: Some(3),
            ..Default::default()
        })
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
results = []
for i in range(5):
    try:
        result = await succeed()
        results.append(f"Success {i}")
    except Exception as e:
        results.append(f"Error {i}: {e}")

for r in results:
    print(r)
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Should complete with limit errors: {:?}",
        result
    );
    let output = result.unwrap();
    // First 3 should succeed, rest should fail
    assert!(output.stdout.contains("Success 0"));
    assert!(output.stdout.contains("Success 1"));
    assert!(output.stdout.contains("Success 2"));
    assert!(output.stdout.contains("Error 3") || output.stdout.contains("Error 4"));
}

#[tokio::test]
async fn test_callback_timeout() {
    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .with_resource_limits(ResourceLimits {
            // Use larger timeout margins to be robust on slow CI runners
            callback_timeout: Some(Duration::from_millis(500)),
            execution_timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        })
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# Short sleep should succeed (50ms sleep with 500ms timeout = plenty of headroom)
try:
    result = await sleep(ms=50)
    print(f"Short sleep: {result}")
except Exception as e:
    print(f"Short sleep error: {e}")

# Long sleep should timeout (1000ms sleep with 500ms timeout = guaranteed timeout)
try:
    result = await sleep(ms=1000)
    print(f"Long sleep: {result}")
except Exception as e:
    print(f"Long sleep timeout: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should handle timeout: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Short sleep:") && output.stdout.contains("slept_ms"),
        "Short sleep should succeed: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("timeout") || output.stdout.contains("Long sleep timeout"),
        "Long sleep should timeout: {}",
        output.stdout
    );
}

// =============================================================================
// Parallel Callback Tests
// =============================================================================

#[tokio::test]
async fn test_parallel_callbacks_with_gather() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio

results = await asyncio.gather(
    echo(message="first"),
    echo(message="second"),
    echo(message="third"),
)

for i, r in enumerate(results):
    print(f"Result {i}: {r['echoed']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Parallel execution should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(output.stdout.contains("first"));
    assert!(output.stdout.contains("second"));
    assert!(output.stdout.contains("third"));
    assert_eq!(output.stats.callback_invocations, 3);
}

#[tokio::test]
async fn test_parallel_callbacks_with_mixed_success_failure() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .with_callback(FailingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio

async def safe_echo(msg):
    try:
        return await echo(message=msg)
    except Exception as e:
        return {"error": str(e)}

async def safe_fail(msg):
    try:
        return await fail(message=msg)
    except Exception as e:
        return {"error": str(e)}

results = await asyncio.gather(
    safe_echo("success1"),
    safe_fail("failure1"),
    safe_echo("success2"),
)

for i, r in enumerate(results):
    print(f"Result {i}: {r}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Mixed parallel should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("success1"));
    assert!(output.stdout.contains("success2"));
    assert!(output.stdout.contains("error"));
}

// =============================================================================
// Edge Cases
// =============================================================================

#[tokio::test]
async fn test_callback_returns_complex_json() {
    struct ComplexCallback;

    impl TypedCallback for ComplexCallback {
        type Args = ();

        fn name(&self) -> &str {
            "complex_json"
        }

        fn description(&self) -> &str {
            "Returns complex JSON structure"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move {
                Ok(json!({
                    "string": "hello",
                    "number": 42,
                    "float": 3.14,
                    "boolean": true,
                    "null": null,
                    "array": [1, 2, 3],
                    "nested": {
                        "a": "b",
                        "c": [4, 5, 6]
                    }
                }))
            })
        }
    }

    let sandbox = sandbox_builder()
        .with_callback(ComplexCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await complex_json()
print(f"string: {result['string']}")
print(f"number: {result['number']}")
print(f"array: {result['array']}")
print(f"nested.a: {result['nested']['a']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Complex JSON should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("string: hello"));
    assert!(output.stdout.contains("number: 42"));
    assert!(output.stdout.contains("[1, 2, 3]"));
    assert!(output.stdout.contains("nested.a: b"));
}

#[tokio::test]
async fn test_callback_with_empty_string_argument() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await echo(message="")
print(f"Echoed empty: '{result['echoed']}'")
"#,
        )
        .await;

    assert!(result.is_ok(), "Empty string should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Echoed empty: ''"));
}

#[tokio::test]
async fn test_callback_with_special_characters() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await echo(message="Line1\nLine2\tTabbed\"Quoted\"")
print(f"Got: {result['echoed']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Special chars should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Line1"));
}

#[tokio::test]
async fn test_sandbox_reuse_across_executions() {
    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .build()
        .expect("Failed to build sandbox");

    // First execution
    let result1 = sandbox.execute("x = await succeed(); print(x)").await;
    assert!(result1.is_ok());

    // Second execution (sandbox reused)
    let result2 = sandbox.execute("y = await succeed(); print(y)").await;
    assert!(result2.is_ok());

    // Third execution
    let result3 = sandbox.execute("z = await succeed(); print(z)").await;
    assert!(result3.is_ok());
}

#[tokio::test]
async fn test_multiple_callbacks_same_sandbox() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .with_callback(SucceedCallback)
        .with_callback(ValidatingCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# Use all three callbacks
e = await echo(message="test")
s = await succeed()
v = await validate(value=50)

print(f"echo: {e}")
print(f"succeed: {s}")
print(f"validate: {v}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Multiple callbacks should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert_eq!(output.stats.callback_invocations, 3);
}

// =============================================================================
// Optional Arguments Tests
// =============================================================================

#[tokio::test]
async fn test_callback_all_optional_args_called_with_none() {
    // Simulates the list_datasources pattern: all parameters are optional,
    // and calling with no arguments should return all items.
    let sandbox = sandbox_builder()
        .with_callback(ListItemsCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await list_items()
print(f"count={result['count']}")
print(f"items={len(result['items'])}")
for item in result['items']:
    print(f"  {item['name']}: {item['type']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "list_items() with no args should succeed: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("count=3"),
        "should return all 3 items when no filters: {}",
        output.stdout
    );
    // Verify item with triple quotes in name is handled correctly
    assert!(
        output.stdout.contains("item'''3"),
        "should handle triple quotes in data: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_all_optional_args_called_with_filter() {
    let sandbox = sandbox_builder()
        .with_callback(ListItemsCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await list_items(types=["prometheus"])
print(f"count={result['count']}")
for item in result['items']:
    print(f"  {item['name']}: {item['type']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "list_items(types=['prometheus']) should succeed: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("count=1"),
        "should return 1 item with prometheus filter: {}",
        output.stdout
    );
}

// =============================================================================
// Positional Argument Tests
// =============================================================================

#[tokio::test]
async fn test_callback_positional_debug_schema() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    // First: inspect the raw schema JSON
    let result = sandbox
        .execute(
            r#"
import json
cbs = list_callbacks()
for cb in cbs:
    print(f"CB: {cb['name']}")
    schema = cb.get('parameters_schema')
    print(f"  parameters_schema={schema}")
    print(f"  type={type(schema)}")
"#,
        )
        .await;

    match &result {
        Ok(output) => eprintln!("SCHEMA STDOUT:\n{}", output.stdout),
        Err(e) => eprintln!("SCHEMA ERROR: {e:?}"),
    }

    // Now try positional arg with error catching
    let result2 = sandbox
        .execute(
            r#"
import traceback
try:
    result = await echo("Hello positional!")
    print(f"Echoed: {result['echoed']}")
except Exception as e:
    print(f"EXCEPTION: {type(e).__name__}: {e}")
    traceback.print_exc()
"#,
        )
        .await;

    match &result2 {
        Ok(output) => {
            eprintln!("POSITIONAL STDOUT:\n{}", output.stdout);
            eprintln!("POSITIONAL STDERR:\n{}", output.stderr);
        }
        Err(e) => eprintln!("POSITIONAL ERROR: {e:?}"),
    }
    assert!(result2.is_ok(), "Should work: {:?}", result2);
}

#[tokio::test]
async fn test_callback_positional_single_arg() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await echo("Hello positional!")
print(f"Echoed: {result['echoed']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Positional arg should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Hello positional!"),
        "Should echo the positional message: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_positional_multiple_args() {
    let sandbox = sandbox_builder()
        .with_callback(AddCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await add(10, 32)
print(f"Result: {result['result']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Multiple positional args should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Result: 42"),
        "add(10, 32) should return 42: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_positional_mixed_with_kwargs() {
    let sandbox = sandbox_builder()
        .with_callback(AddCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await add(10, b=32)
print(f"Result: {result['result']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Mixed positional and keyword args should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Result: 42"),
        "add(10, b=32) should return 42: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_positional_namespace() {
    let sandbox = sandbox_builder()
        .with_callback(SearchDashboardsCallback)
        .build()
        .expect("Failed to build sandbox");

    // This is the exact pattern that was failing before the fix:
    // search.dashboards('overview') passes 'overview' as a positional arg
    let result = sandbox
        .execute(
            r#"
result = await search.dashboards('overview')
print(f"Dashboards: {result['dashboards']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Namespace callback with positional arg should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("overview"),
        "search.dashboards('overview') should return results: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_keyword_args_still_work() {
    let sandbox = sandbox_builder()
        .with_callback(AddCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await add(a=100, b=200)
print(f"Result: {result['result']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Keyword args should still work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Result: 300"),
        "add(a=100, b=200) should return 300: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_positional_namespace_keyword_fallback() {
    let sandbox = sandbox_builder()
        .with_callback(SearchDashboardsCallback)
        .build()
        .expect("Failed to build sandbox");

    // Keyword args should still work for namespace callbacks
    let result = sandbox
        .execute(
            r#"
result = await search.dashboards(query='metrics')
print(f"Dashboards: {result['dashboards']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Namespace callback with keyword arg should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("metrics"),
        "search.dashboards(query='metrics') should return results: {}",
        output.stdout
    );
}

// =============================================================================
// Duplicate Positional/Keyword Argument Tests
// =============================================================================

#[tokio::test]
async fn test_callback_positional_and_keyword_duplicate_raises_typeerror() {
    let sandbox = sandbox_builder()
        .with_callback(AddCallback)
        .build()
        .expect("Failed to build sandbox");

    // Passing 'a' as both positional (first arg) and keyword should raise TypeError
    let result = sandbox
        .execute(
            r#"
try:
    result = await add(1, a=2)
    print("ERROR: should have raised TypeError")
except TypeError as e:
    print(f"TypeError: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("TypeError"),
        "Should raise TypeError for duplicate arg: {}",
        output.stdout
    );
    assert!(
        output
            .stdout
            .contains("got multiple values for argument 'a'"),
        "Error message should mention the parameter name: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_callback_namespace_positional_and_keyword_duplicate_raises_typeerror() {
    let sandbox = sandbox_builder()
        .with_callback(SearchDashboardsCallback)
        .build()
        .expect("Failed to build sandbox");

    // Passing 'query' as both positional and keyword via namespace leaf
    let result = sandbox
        .execute(
            r#"
try:
    result = await search.dashboards('test', query='other')
    print("ERROR: should have raised TypeError")
except TypeError as e:
    print(f"TypeError: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("TypeError"),
        "Should raise TypeError for duplicate arg in namespace callback: {}",
        output.stdout
    );
    assert!(
        output
            .stdout
            .contains("got multiple values for argument 'query'"),
        "Error message should mention the parameter name: {}",
        output.stdout
    );
}
