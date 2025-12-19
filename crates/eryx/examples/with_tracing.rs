//! Example demonstrating execution tracing and output handling.
//!
//! This example shows how to use `TraceHandler` and `OutputHandler` to
//! monitor Python execution in real-time.
//!
//! Run with: `cargo run --example with_tracing`

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eryx::JsonSchema;
use eryx::{
    CallbackError, OutputHandler, Sandbox, TraceEvent, TraceEventKind, TraceHandler, TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Arguments for the slow operation callback.
#[derive(Deserialize, JsonSchema)]
struct SlowOperationArgs {
    /// A value to process
    value: i64,
}

/// A callback that simulates some work with a delay.
struct SlowCallback {
    delay_ms: u64,
}

impl TypedCallback for SlowCallback {
    type Args = SlowOperationArgs;

    fn name(&self) -> &str {
        "slow_operation"
    }

    fn description(&self) -> &str {
        "Simulates a slow operation that takes time to complete"
    }

    fn invoke_typed(
        &self,
        args: SlowOperationArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let delay_ms = self.delay_ms;
        Box::pin(async move {
            // Simulate work
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;

            Ok(json!({ "result": args.value * 2 }))
        })
    }
}

/// A trace handler that prints trace events to stdout.
struct PrintingTraceHandler {
    event_count: AtomicU32,
}

impl PrintingTraceHandler {
    const fn new() -> Self {
        Self {
            event_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl TraceHandler for PrintingTraceHandler {
    async fn on_trace(&self, event: TraceEvent) {
        let count = self.event_count.fetch_add(1, Ordering::SeqCst) + 1;

        let event_desc = match &event.event {
            TraceEventKind::Line => format!("line {}", event.lineno),
            TraceEventKind::Call { function } => format!("call {function}()"),
            TraceEventKind::Return { function } => format!("return from {function}()"),
            TraceEventKind::Exception { message } => format!("exception: {message}"),
            TraceEventKind::CallbackStart { name } => format!("callback START: {name}"),
            TraceEventKind::CallbackEnd { name, duration_ms } => {
                format!("callback END: {name} ({duration_ms}ms)")
            }
        };

        println!("  [{count:3}] {event_desc}");
    }
}

/// An output handler that prints output in real-time with a prefix.
struct StreamingOutputHandler;

#[async_trait]
impl OutputHandler for StreamingOutputHandler {
    async fn on_output(&self, output: &str) {
        for line in output.lines() {
            println!("  [OUTPUT] {line}");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build the sandbox with tracing enabled using embedded runtime
    let sandbox = Sandbox::embedded()
        .with_callback(SlowCallback { delay_ms: 50 })
        .with_trace_handler(PrintingTraceHandler::new())
        .with_output_handler(StreamingOutputHandler)
        .build()?;

    println!("Sandbox created with tracing enabled");
    println!();

    // Example 1: Simple code with tracing
    println!("=== Example 1: Trace simple code execution ===");
    println!("Executing Python code with trace events:");
    let result = sandbox
        .execute(
            "
x = 10
y = 20
result = x + y
print(f'The result is: {result}')
",
        )
        .await?;

    println!();
    println!("Execution complete!");
    println!("  Duration: {:?}", result.stats.duration);
    println!("  Trace events collected: {}", result.trace.len());
    println!();

    // Example 2: Code with callbacks (shows callback start/end traces)
    println!("=== Example 2: Trace callback execution ===");
    println!("Executing Python code with callback invocations:");
    let result = sandbox
        .execute(
            r#"
print("Starting slow operation...")
result = await slow_operation(value=21)
print(f"Got result: {result}")
"#,
        )
        .await?;

    println!();
    println!("Execution complete!");
    println!("  Duration: {:?}", result.stats.duration);
    println!("  Callbacks invoked: {}", result.stats.callback_invocations);
    println!();

    // Example 3: Parallel callbacks with tracing
    println!("=== Example 3: Trace parallel callbacks ===");
    println!("Executing multiple callbacks in parallel:");
    let result = sandbox
        .execute(
            r#"
import asyncio

print("Starting 3 parallel operations...")
results = await asyncio.gather(
    slow_operation(value=1),
    slow_operation(value=2),
    slow_operation(value=3),
)
print(f"All done! Results: {results}")
"#,
        )
        .await?;

    println!();
    println!("Execution complete!");
    println!("  Duration: {:?}", result.stats.duration);
    println!("  Callbacks invoked: {}", result.stats.callback_invocations);
    println!();

    // Show trace event summary
    println!("=== Trace Event Summary ===");
    let mut callback_starts = 0;
    let mut callback_ends = 0;
    let mut lines = 0;
    let mut calls = 0;
    let mut returns = 0;

    for event in &result.trace {
        match &event.event {
            TraceEventKind::Line => lines += 1,
            TraceEventKind::Call { .. } => calls += 1,
            TraceEventKind::Return { .. } => returns += 1,
            TraceEventKind::CallbackStart { .. } => callback_starts += 1,
            TraceEventKind::CallbackEnd { .. } => callback_ends += 1,
            TraceEventKind::Exception { .. } => {}
        }
    }

    println!("  Line events: {lines}");
    println!("  Call events: {calls}");
    println!("  Return events: {returns}");
    println!("  Callback start events: {callback_starts}");
    println!("  Callback end events: {callback_ends}");
    println!();

    println!("Tracing example completed successfully!");

    Ok(())
}
