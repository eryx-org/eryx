//! Example demonstrating parallel callback execution via asyncio.gather.
//!
//! This example shows that callbacks are executed concurrently, not sequentially.
//! If 3 callbacks each take 100ms and run in parallel, total time should be ~100ms.
//! If they ran sequentially, it would take ~300ms.
//!
//! Run with: `cargo run --example parallel_callbacks`

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use eryx::JsonSchema;
use eryx::{CallbackError, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

/// Arguments for the sleep callback.
#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Duration to sleep in milliseconds
    ms: u64,
}

/// A callback that sleeps for a specified duration.
struct SleepCallback {
    /// Track concurrent execution count
    concurrent_count: Arc<AtomicU32>,
    /// Track peak concurrent execution
    peak_concurrent: Arc<AtomicU32>,
}

impl SleepCallback {
    fn new() -> Self {
        Self {
            concurrent_count: Arc::new(AtomicU32::new(0)),
            peak_concurrent: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl TypedCallback for SleepCallback {
    type Args = SleepArgs;

    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleeps for the specified number of milliseconds"
    }

    fn invoke_typed(
        &self,
        args: SleepArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        // Increment concurrent count at start
        let current = self.concurrent_count.fetch_add(1, Ordering::SeqCst) + 1;

        // Update peak if this is a new maximum
        self.peak_concurrent.fetch_max(current, Ordering::SeqCst);

        let concurrent_count = self.concurrent_count.clone();

        Box::pin(async move {
            // Sleep for the specified duration
            tokio::time::sleep(tokio::time::Duration::from_millis(args.ms)).await;

            // Decrement concurrent count at end
            concurrent_count.fetch_sub(1, Ordering::SeqCst);

            Ok(json!({ "slept_ms": args.ms }))
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Parallel Execution Test ===\n");

    let sleep_callback = SleepCallback::new();
    let peak_concurrent = sleep_callback.peak_concurrent.clone();

    let sandbox = Sandbox::embedded().with_callback(sleep_callback).build()?;

    // Test 1: Sequential execution (baseline)
    println!("Test 1: Sequential execution (3 x 100ms callbacks)");
    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
result1 = await sleep(ms=100)
result2 = await sleep(ms=100)
result3 = await sleep(ms=100)
print(f"Results: {result1}, {result2}, {result3}")
"#,
        )
        .await?;
    let sequential_duration = start.elapsed();

    println!("  Duration: {sequential_duration:?}");
    println!("  Output: {}", result.stdout);
    println!("  Callbacks invoked: {}", result.stats.callback_invocations);
    println!();

    // Test 2: Parallel execution
    println!("Test 2: Parallel execution (3 x 100ms callbacks via asyncio.gather)");
    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
import asyncio
results = await asyncio.gather(
    sleep(ms=100),
    sleep(ms=100),
    sleep(ms=100),
)
print(f"Results: {results}")
"#,
        )
        .await?;
    let parallel_duration = start.elapsed();

    println!("  Duration: {parallel_duration:?}");
    println!("  Output: {}", result.stdout);
    println!("  Callbacks invoked: {}", result.stats.callback_invocations);
    println!(
        "  Peak concurrent callbacks: {}",
        peak_concurrent.load(Ordering::SeqCst)
    );
    println!();

    // Test 3: Verify timing
    println!("=== Results ===");
    println!("Sequential duration: {sequential_duration:?}");
    println!("Parallel duration:   {parallel_duration:?}");

    let speedup = sequential_duration.as_millis() as f64 / parallel_duration.as_millis() as f64;
    println!("Speedup: {speedup:.2}x");

    // Verify parallel execution is actually faster
    let sequential_ms = sequential_duration.as_millis();
    let parallel_ms = parallel_duration.as_millis();

    println!();
    if parallel_ms < 200 && sequential_ms > 250 {
        println!("✅ PASS: Parallel execution is working!");
        println!("   Sequential took {sequential_ms}ms (expected ~300ms)");
        println!("   Parallel took {parallel_ms}ms (expected ~100ms)");
    } else if parallel_ms >= sequential_ms {
        println!("❌ FAIL: Parallel execution is NOT faster than sequential!");
        println!("   This suggests callbacks are running sequentially.");
    } else {
        println!("⚠️  INCONCLUSIVE: Results are close, may need longer sleep times");
        println!("   Sequential: {sequential_ms}ms, Parallel: {parallel_ms}ms");
    }

    // Check peak concurrency
    let peak = peak_concurrent.load(Ordering::SeqCst);
    println!();
    if peak >= 3 {
        println!("✅ PASS: Peak concurrent callbacks = {peak} (3 ran simultaneously)");
    } else {
        println!("⚠️  Peak concurrent callbacks = {peak} (expected 3 for full parallelism)");
    }

    Ok(())
}
