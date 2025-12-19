//! Precise trace event tests.
//!
//! These tests document exactly what trace events are emitted for various
//! Python scripts, using exact `assert_eq!` comparisons.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eryx::{
    CallbackError, JsonSchema, Sandbox, TraceEvent, TraceEventKind, TraceHandler, TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Test Infrastructure
// =============================================================================

/// Simplified trace event for assertions (ignores duration_ms in CallbackEnd).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SimpleEvent {
    lineno: u32,
    kind: String,
}

impl SimpleEvent {
    fn new(lineno: u32, kind: &str) -> Self {
        Self {
            lineno,
            kind: kind.to_string(),
        }
    }

    fn line(lineno: u32) -> Self {
        Self::new(lineno, "line")
    }

    fn call(lineno: u32, function: &str) -> Self {
        Self::new(lineno, &format!("call:{}", function))
    }

    fn ret(lineno: u32, function: &str) -> Self {
        Self::new(lineno, &format!("return:{}", function))
    }

    fn callback_start(lineno: u32, name: &str) -> Self {
        Self::new(lineno, &format!("callback_start:{}", name))
    }

    fn callback_end(lineno: u32, name: &str) -> Self {
        Self::new(lineno, &format!("callback_end:{}", name))
    }
}

impl From<&TraceEvent> for SimpleEvent {
    fn from(event: &TraceEvent) -> Self {
        let kind = match &event.event {
            TraceEventKind::Line => "line".to_string(),
            TraceEventKind::Call { function } => format!("call:{}", function),
            TraceEventKind::Return { function } => format!("return:{}", function),
            TraceEventKind::Exception { message } => format!("exception:{}", message),
            TraceEventKind::CallbackStart { name } => format!("callback_start:{}", name),
            TraceEventKind::CallbackEnd { name, .. } => format!("callback_end:{}", name),
        };
        Self {
            lineno: event.lineno,
            kind,
        }
    }
}

/// Shared data for trace collection.
#[derive(Default)]
struct TraceData {
    events: Vec<TraceEvent>,
}

/// Collects trace events for verification.
#[derive(Clone, Default)]
struct CollectingTraceHandler {
    data: Arc<Mutex<TraceData>>,
}

impl CollectingTraceHandler {
    fn new() -> Self {
        Self::default()
    }

    fn simple_events(&self) -> Vec<SimpleEvent> {
        self.data
            .lock()
            .unwrap()
            .events
            .iter()
            .map(SimpleEvent::from)
            .collect()
    }
}

#[async_trait]
impl TraceHandler for CollectingTraceHandler {
    async fn on_trace(&self, event: TraceEvent) {
        self.data.lock().unwrap().events.push(event);
    }
}

// Test callbacks

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

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    message: String,
}

struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the message"
    }

    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"echoed": args.message})) })
    }
}

// Helper to create sandbox

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
// Precise Trace Event Tests
// =============================================================================

/// Test: Simple assignment `x = 1`
///
/// Expected trace:
/// ```text
/// L0: call:<module>
/// L1: line
/// L1: return:<module>
/// ```
#[tokio::test]
async fn test_trace_simple_assignment() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox.execute("x = 1").await;
    assert!(result.is_ok(), "Execution should succeed");

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::ret(1, "<module>"),
        ]
    );
}

/// Test: Multiple statements
///
/// Code:
/// ```python
/// x = 1
/// y = 2
/// z = x + y
/// ```
///
/// Expected trace:
/// ```text
/// L0: call:<module>
/// L1: line
/// L2: line
/// L3: line
/// L3: return:<module>
/// ```
#[tokio::test]
async fn test_trace_multiple_statements() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            "x = 1
y = 2
z = x + y",
        )
        .await;
    assert!(result.is_ok(), "Execution should succeed");

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::line(2),
            SimpleEvent::line(3),
            SimpleEvent::ret(3, "<module>"),
        ]
    );
}

/// Test: Function definition and call
///
/// Code:
/// ```python
/// def foo():
///     return 42
///
/// result = foo()
/// ```
///
/// Expected trace:
/// ```text
/// L0: call:<module>
/// L1: line          (def foo)
/// L4: line          (result = foo())
/// L1: call:foo
/// L2: line          (return 42)
/// L2: return:foo
/// L4: return:<module>
/// ```
#[tokio::test]
async fn test_trace_function_call() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!("def foo():\n", "    return 42\n", "\n", "result = foo()");

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // def foo
            SimpleEvent::line(4), // result = foo()
            SimpleEvent::call(1, "foo"),
            SimpleEvent::line(2), // return 42
            SimpleEvent::ret(2, "foo"),
            SimpleEvent::ret(4, "<module>"),
        ]
    );
}

/// Test: Function calling another function (defined first)
///
/// Code:
/// ```python
/// def helper():
///     return 1
///
/// x = helper()
/// ```
#[tokio::test]
async fn test_trace_two_function_calls() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!(
        "def helper():\n",
        "    return 1\n",
        "\n",
        "x = helper()\n",
        "y = helper()"
    );

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // def helper
            SimpleEvent::line(4), // x = helper()
            SimpleEvent::call(1, "helper"),
            SimpleEvent::line(2), // return 1
            SimpleEvent::ret(2, "helper"),
            SimpleEvent::line(5), // y = helper()
            SimpleEvent::call(1, "helper"),
            SimpleEvent::line(2), // return 1
            SimpleEvent::ret(2, "helper"),
            SimpleEvent::ret(5, "<module>"),
        ]
    );
}

/// Test: Callback invocation
///
/// Code:
/// ```python
/// result = await succeed()
/// ```
///
/// Note: callback_start/callback_end events are reported explicitly by invoke().
/// The module return happens when the coroutine yields for the await.
#[tokio::test]
async fn test_trace_callback_invocation() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox.execute("result = await succeed()").await;
    assert!(result.is_ok(), "Execution should succeed");

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::callback_start(0, "succeed"),
            SimpleEvent::ret(1, "<module>"),
            SimpleEvent::callback_end(0, "succeed"),
        ]
    );
}

/// Test: Multiple sequential callbacks
///
/// Code:
/// ```python
/// a = await succeed()
/// b = await succeed()
/// ```
#[tokio::test]
async fn test_trace_multiple_callbacks() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(SucceedCallback)
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            "a = await succeed()
b = await succeed()",
        )
        .await;
    assert!(result.is_ok(), "Execution should succeed");

    let events = trace.simple_events();

    // Note: With async callbacks, the trace shows callback_start/callback_end
    // for each callback, but the second callback happens after the coroutine
    // yields and doesn't trigger another line event due to async execution flow.
    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::callback_start(0, "succeed"),
            SimpleEvent::ret(1, "<module>"),
            SimpleEvent::callback_end(0, "succeed"),
            SimpleEvent::callback_start(0, "succeed"),
            SimpleEvent::callback_end(0, "succeed"),
        ]
    );
}

/// Test: Callback with arguments
///
/// Code:
/// ```python
/// result = await echo(message="test")
/// ```
#[tokio::test]
async fn test_trace_callback_with_args() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(r#"result = await echo(message="test")"#)
        .await;
    assert!(result.is_ok(), "Execution should succeed");

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::callback_start(0, "echo"),
            SimpleEvent::ret(1, "<module>"),
            SimpleEvent::callback_end(0, "echo"),
        ]
    );
}

/// Test: Loop with 3 iterations
///
/// Code:
/// ```python
/// total = 0
/// for i in range(3):
///     total += i
/// ```
#[tokio::test]
async fn test_trace_loop() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!("total = 0\n", "for i in range(3):\n", "    total += i");

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // total = 0
            SimpleEvent::line(2), // for i in range(3)
            SimpleEvent::line(3), // total += i (iteration 0)
            SimpleEvent::line(2), // for (check next)
            SimpleEvent::line(3), // total += i (iteration 1)
            SimpleEvent::line(2), // for (check next)
            SimpleEvent::line(3), // total += i (iteration 2)
            SimpleEvent::line(2), // for (loop exhausted)
            SimpleEvent::ret(2, "<module>"),
        ]
    );
}

/// Test: Conditional (if/else) - true branch taken
///
/// Code:
/// ```python
/// x = 10
/// if x > 5:
///     y = 1
/// else:
///     y = 0
/// ```
#[tokio::test]
async fn test_trace_conditional_true_branch() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!(
        "x = 10\n",
        "if x > 5:\n",
        "    y = 1\n",
        "else:\n",
        "    y = 0"
    );

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // x = 10
            SimpleEvent::line(2), // if x > 5
            SimpleEvent::line(3), // y = 1 (true branch)
            SimpleEvent::ret(3, "<module>"),
        ]
    );
}

/// Test: Conditional (if/else) - false branch taken
///
/// Code:
/// ```python
/// x = 1
/// if x > 5:
///     y = 1
/// else:
///     y = 0
/// ```
#[tokio::test]
async fn test_trace_conditional_false_branch() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!(
        "x = 1\n",
        "if x > 5:\n",
        "    y = 1\n",
        "else:\n",
        "    y = 0"
    );

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // x = 1
            SimpleEvent::line(2), // if x > 5
            SimpleEvent::line(5), // y = 0 (false branch)
            SimpleEvent::ret(5, "<module>"),
        ]
    );
}

/// Test: Function with multiple statements
///
/// Code:
/// ```python
/// def compute(a, b):
///     sum = a + b
///     product = a * b
///     return sum + product
///
/// result = compute(2, 3)
/// ```
#[tokio::test]
async fn test_trace_function_multiple_statements() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    // Use concat! to avoid indentation issues
    let code = concat!(
        "def compute(a, b):\n",
        "    total = a + b\n",
        "    product = a * b\n",
        "    return total + product\n",
        "\n",
        "result = compute(2, 3)"
    );

    let result = sandbox.execute(code).await;
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1), // def compute
            SimpleEvent::line(6), // result = compute(2, 3)
            SimpleEvent::call(1, "compute"),
            SimpleEvent::line(2), // total = a + b
            SimpleEvent::line(3), // product = a * b
            SimpleEvent::line(4), // return total + product
            SimpleEvent::ret(4, "compute"),
            SimpleEvent::ret(6, "<module>"),
        ]
    );
}

/// Test: Print produces line event (not a separate output trace)
///
/// Code:
/// ```python
/// print("hello")
/// ```
#[tokio::test]
async fn test_trace_print() {
    let trace = CollectingTraceHandler::new();

    let sandbox = sandbox_builder()
        .with_trace_handler(trace.clone())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox.execute("print('hello')").await;
    assert!(result.is_ok(), "Execution should succeed");

    let output = result.unwrap();
    assert_eq!(output.stdout, "hello");

    let events = trace.simple_events();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::ret(1, "<module>"),
        ]
    );
}

/// Test: Trace events are in ExecuteResult.trace as well
#[tokio::test]
async fn test_trace_events_in_result() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let result = sandbox.execute("x = 1").await;
    assert!(result.is_ok(), "Execution should succeed");

    let output = result.unwrap();
    let events: Vec<SimpleEvent> = output.trace.iter().map(SimpleEvent::from).collect();

    assert_eq!(
        events,
        vec![
            SimpleEvent::call(0, "<module>"),
            SimpleEvent::line(1),
            SimpleEvent::ret(1, "<module>"),
        ]
    );
}
