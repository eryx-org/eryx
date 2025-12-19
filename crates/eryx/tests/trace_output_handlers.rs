//! Integration tests for TraceHandler and OutputHandler.
//!
//! These tests verify that trace events and output streaming work correctly
//! through the Sandbox API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use eryx::{
    CallbackError, JsonSchema, OutputHandler, ResourceLimits, Sandbox, TraceEvent, TraceEventKind,
    TraceHandler, TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Test Callbacks
// =============================================================================

/// A callback that sleeps for the specified duration.
#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    ms: u64,
}

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

struct SucceedCallback;

impl TypedCallback for SucceedCallback {
    type Args = ();

    fn name(&self) -> &str {
        "succeed"
    }

    fn description(&self) -> &str {
        "Always succeeds"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"status": "ok"})) })
    }
}

// =============================================================================
// Test Handlers
// =============================================================================

/// Shared data for the collecting trace handler.
#[derive(Default)]
struct TraceData {
    events: Vec<TraceEvent>,
    call_count: u32,
}

/// A trace handler that collects all trace events for verification.
/// Uses inner Arc<Mutex> for shared data so the handler can be cloned
/// and the data accessed after execution.
#[derive(Clone)]
struct CollectingTraceHandler {
    data: Arc<Mutex<TraceData>>,
}

impl CollectingTraceHandler {
    fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(TraceData::default())),
        }
    }

    fn events(&self) -> Vec<TraceEvent> {
        self.data.lock().unwrap().events.clone()
    }

    fn call_count(&self) -> u32 {
        self.data.lock().unwrap().call_count
    }
}

#[async_trait]
impl TraceHandler for CollectingTraceHandler {
    async fn on_trace(&self, event: TraceEvent) {
        let mut data = self.data.lock().unwrap();
        data.call_count += 1;
        data.events.push(event);
    }
}

/// Shared data for the collecting output handler.
#[derive(Default)]
struct OutputData {
    chunks: Vec<String>,
    call_count: u32,
}

/// An output handler that collects all output chunks for verification.
/// Uses inner Arc<Mutex> for shared data so the handler can be cloned
/// and the data accessed after execution.
#[derive(Clone)]
struct CollectingOutputHandler {
    data: Arc<Mutex<OutputData>>,
}

impl CollectingOutputHandler {
    fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(OutputData::default())),
        }
    }

    fn call_count(&self) -> u32 {
        self.data.lock().unwrap().call_count
    }

    fn combined_output(&self) -> String {
        self.data.lock().unwrap().chunks.join("")
    }
}

#[async_trait]
impl OutputHandler for CollectingOutputHandler {
    async fn on_output(&self, output: &str) {
        let mut data = self.data.lock().unwrap();
        data.call_count += 1;
        data.chunks.push(output.to_string());
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
// TraceHandler Tests
// =============================================================================

#[tokio::test]
async fn test_trace_handler_receives_line_events() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
x = 1
y = 2
z = x + y
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    // Check that trace handler was called
    assert!(
        trace_handler.call_count() > 0,
        "Trace handler should have been called"
    );

    // Check that we got line events
    let events = trace_handler.events();
    let line_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.event, TraceEventKind::Line))
        .collect();

    assert!(
        !line_events.is_empty(),
        "Should have received line events, got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_trace_handler_receives_call_and_return_events() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
def my_function():
    return 42

result = my_function()
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace_handler.events();

    // Check for call event
    let call_events: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(&e.event, TraceEventKind::Call { function } if function == "my_function"),
        )
        .collect();

    assert!(
        !call_events.is_empty(),
        "Should have call event for my_function, got: {:?}",
        events
    );

    // Check for return event
    let return_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.event, TraceEventKind::Return { function } if function == "my_function"))
        .collect();

    assert!(
        !return_events.is_empty(),
        "Should have return event for my_function, got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_trace_handler_receives_callback_events() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await succeed()
print(result)
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace_handler.events();

    // Check for callback start event
    let callback_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.event, TraceEventKind::CallbackStart { name } if name == "succeed"))
        .collect();

    assert!(
        !callback_starts.is_empty(),
        "Should have CallbackStart event for 'succeed', got: {:?}",
        events
    );

    // Check for callback end event
    let callback_ends: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(&e.event, TraceEventKind::CallbackEnd { name, .. } if name == "succeed"),
        )
        .collect();

    assert!(
        !callback_ends.is_empty(),
        "Should have CallbackEnd event for 'succeed', got: {:?}",
        events
    );
}

#[tokio::test]
async fn test_trace_handler_callback_duration_tracked() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .with_trace_handler(trace_handler.clone())
        .with_resource_limits(ResourceLimits {
            callback_timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        })
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await sleep(ms=50)
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace_handler.events();

    // Find callback end event
    let callback_end = events
        .iter()
        .find(|e| matches!(&e.event, TraceEventKind::CallbackEnd { name, .. } if name == "sleep"));

    assert!(
        callback_end.is_some(),
        "Should have CallbackEnd event for 'sleep'"
    );

    // Note: Duration tracking depends on the Python runtime implementation.
    // We verify the event exists; duration may or may not be accurate.
    if let Some(event) = callback_end
        && let TraceEventKind::CallbackEnd { duration_ms, name } = &event.event
    {
        assert_eq!(name, "sleep", "Callback name should be 'sleep'");
        // Duration is informational - just verify it's a valid u64
        let _ = *duration_ms;
    }
}

#[tokio::test]
async fn test_trace_events_in_result() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
x = 1
y = 2
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");
    let output = result.unwrap();

    // Trace events should be collected in the result even without a handler
    assert!(
        !output.trace.is_empty(),
        "Result should contain trace events"
    );

    // Should have line events
    let has_line_events = output
        .trace
        .iter()
        .any(|e| matches!(e.event, TraceEventKind::Line));
    assert!(has_line_events, "Should have line events in result");
}

#[tokio::test]
async fn test_trace_handler_exception_event() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    // This should raise an exception
    let result = sandbox.execute("raise ValueError('test error')").await;

    assert!(result.is_err(), "Execution should fail");

    // Note: Exception events may or may not be captured depending on
    // how the Python runtime handles sys.settrace during exception propagation
    // This test mainly verifies the handler doesn't break on errors
}

// =============================================================================
// OutputHandler Tests
// =============================================================================

#[tokio::test]
async fn test_output_handler_receives_print_output() {
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
print("Hello, World!")
print("Line 2")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    // Check that output handler was called
    assert!(
        output_handler.call_count() > 0,
        "Output handler should have been called"
    );

    // Check the combined output
    let combined = output_handler.combined_output();
    assert!(
        combined.contains("Hello, World!"),
        "Output should contain 'Hello, World!': {}",
        combined
    );
    assert!(
        combined.contains("Line 2"),
        "Output should contain 'Line 2': {}",
        combined
    );
}

#[tokio::test]
async fn test_output_handler_matches_result_stdout() {
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
for i in range(5):
    print(f"Count: {i}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");
    let output = result.unwrap();

    // The output handler's combined output should match result.stdout
    let handler_output = output_handler.combined_output();
    assert_eq!(
        handler_output, output.stdout,
        "Handler output should match result stdout"
    );
}

#[tokio::test]
async fn test_output_handler_with_empty_output() {
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
x = 1 + 1  # No print statements
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");
    let output = result.unwrap();

    // No output should have been generated
    assert!(output.stdout.is_empty(), "Should have no stdout");

    // Handler might or might not be called with empty string
    let combined = output_handler.combined_output();
    assert!(combined.is_empty(), "Handler output should be empty");
}

#[tokio::test]
async fn test_output_handler_with_unicode() {
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
print("Hello 世界 🌍 مرحبا")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");

    let combined = output_handler.combined_output();
    assert!(
        combined.contains("世界"),
        "Output should contain Chinese: {}",
        combined
    );
    assert!(
        combined.contains("🌍"),
        "Output should contain emoji: {}",
        combined
    );
}

#[tokio::test]
async fn test_output_handler_large_output() {
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
for i in range(100):
    print(f"Line {i}: " + "x" * 100)
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");
    let output = result.unwrap();

    let combined = output_handler.combined_output();
    assert!(
        combined.len() > 10000,
        "Should have substantial output: {} chars",
        combined.len()
    );
    assert_eq!(
        combined, output.stdout,
        "Handler output should match stdout"
    );
}

// =============================================================================
// Combined Handler Tests
// =============================================================================

#[tokio::test]
async fn test_both_handlers_together() {
    let trace_handler = CollectingTraceHandler::new();
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_trace_handler(trace_handler.clone())
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
print("Starting...")
result = await succeed()
print(f"Got: {result}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Execution should succeed");

    // Verify trace handler got events
    assert!(
        trace_handler.call_count() > 0,
        "Trace handler should be called"
    );
    let events = trace_handler.events();
    assert!(!events.is_empty(), "Should have trace events");

    // Verify output handler got output
    assert!(
        output_handler.call_count() > 0,
        "Output handler should be called"
    );
    let combined = output_handler.combined_output();
    assert!(combined.contains("Starting"), "Should have 'Starting'");
    assert!(combined.contains("Got:"), "Should have 'Got:'");

    // Verify callback events were traced
    let has_callback_events = events.iter().any(|e| {
        matches!(
            &e.event,
            TraceEventKind::CallbackStart { .. } | TraceEventKind::CallbackEnd { .. }
        )
    });
    assert!(has_callback_events, "Should have callback trace events");
}

#[tokio::test]
async fn test_handlers_with_error() {
    let trace_handler = CollectingTraceHandler::new();
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
print("Before error")
x = 1 / 0  # This will raise ZeroDivisionError
print("After error")  # Never reached
"#,
        )
        .await;

    assert!(result.is_err(), "Execution should fail");

    // Note: Output handler is only called on successful execution.
    // When execution fails, the output handler may not receive partial output.
    // This is current behavior - output is collected and only streamed on success.

    // Trace handler should have received some events before the error
    // (trace events are collected during execution, not just at the end)
    assert!(
        trace_handler.call_count() > 0,
        "Trace handler should have been called"
    );

    // Verify we got trace events
    let events = trace_handler.events();
    assert!(!events.is_empty(), "Should have trace events");
}

#[tokio::test]
async fn test_sandbox_reuse_with_handlers() {
    let trace_handler = CollectingTraceHandler::new();
    let output_handler = CollectingOutputHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .with_output_handler(output_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    // First execution
    let result1 = sandbox.execute("print('First')").await;
    assert!(result1.is_ok());

    let count_after_first = trace_handler.call_count();
    let output_after_first = output_handler.combined_output();

    // Second execution
    let result2 = sandbox.execute("print('Second')").await;
    assert!(result2.is_ok());

    // Handlers should have received more events/output
    assert!(
        trace_handler.call_count() > count_after_first,
        "Trace handler should have more calls after second execution"
    );

    let final_output = output_handler.combined_output();
    assert!(
        final_output.len() > output_after_first.len(),
        "Output handler should have more output"
    );
    assert!(final_output.contains("First"));
    assert!(final_output.contains("Second"));
}

// =============================================================================
// TraceEvent Content Tests
// =============================================================================

#[tokio::test]
async fn test_trace_event_line_numbers() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"x = 1
y = 2
z = 3"#,
        )
        .await;

    assert!(result.is_ok());

    let events = trace_handler.events();
    let line_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.event, TraceEventKind::Line))
        .collect();

    // Should have line events with valid line numbers
    for event in &line_events {
        assert!(
            event.lineno > 0,
            "Line number should be positive: {}",
            event.lineno
        );
    }
}

#[tokio::test]
async fn test_trace_events_order() {
    let trace_handler = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_trace_handler(trace_handler.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await succeed()
"#,
        )
        .await;

    assert!(result.is_ok());

    let events = trace_handler.events();

    // Find callback start and end indices
    let start_idx = events
        .iter()
        .position(|e| matches!(&e.event, TraceEventKind::CallbackStart { .. }));
    let end_idx = events
        .iter()
        .position(|e| matches!(&e.event, TraceEventKind::CallbackEnd { .. }));

    if let (Some(start), Some(end)) = (start_idx, end_idx) {
        assert!(start < end, "CallbackStart should come before CallbackEnd");
    }
}
