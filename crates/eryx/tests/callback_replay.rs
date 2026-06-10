//! End-to-end integration tests for callback-result replay.
//!
//! These exercise [`Sandbox::execute_with_journal`] through the full WASM
//! Python runtime: a journal is recorded on a first run and then replayed on a
//! second run, proving that matching callbacks are served from cache (the real
//! callback is not invoked).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eryx::{Callback, CallbackError, Sandbox, Schema};
use serde_json::{Value, json};

// =============================================================================
// Test callbacks
// =============================================================================

/// A callback that counts how many times it is *actually* invoked and returns
/// that count, so a replayed (cached) result is distinguishable from a fresh
/// live call.
struct CountingCallback {
    name: String,
    live_calls: Arc<AtomicU32>,
}

impl Callback for CountingCallback {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Counts live invocations"
    }
    fn parameters_schema(&self) -> Schema {
        Schema::empty()
    }
    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        let n = self.live_calls.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(json!({ "live_call": n, "args": args })) })
    }
}

/// A callback that sleeps for a configurable duration before returning, so a
/// concurrent `asyncio.gather` batch completes in a different order than it was
/// initiated. Counts live invocations like [`CountingCallback`].
struct SlowCallback {
    name: String,
    delay_ms: u64,
    live_calls: Arc<AtomicU32>,
}

impl Callback for SlowCallback {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Sleeps then returns its name"
    }
    fn parameters_schema(&self) -> Schema {
        Schema::empty()
    }
    fn invoke(
        &self,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay_ms;
        let name = self.name.clone();
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            Ok(json!({ "from": name }))
        })
    }
}

// =============================================================================
// Helpers
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
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }
    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

// =============================================================================
// Tests
// =============================================================================

const TWO_CALL_SCRIPT: &str = r#"
a = await tick(step="one")
b = await tick(step="two")
print(f"a={a['live_call']} b={b['live_call']}")
"#;

/// A first run records a journal; a second run with that journal replays both
/// callbacks from cache without invoking the real callback again.
#[tokio::test]
async fn replay_serves_cached_callbacks_without_invoking() {
    let live_calls = Arc::new(AtomicU32::new(0));

    // ---- First run: record the journal. ----
    let sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&live_calls),
        })
        .build()
        .expect("build sandbox");

    let first = sandbox.execute_with_journal(TWO_CALL_SCRIPT).await;
    first.result.expect("first run succeeds");
    assert_eq!(live_calls.load(Ordering::SeqCst), 2, "two live calls");
    assert_eq!(first.journal.entries.len(), 2, "two journaled callbacks");
    assert_eq!(first.replayed_callbacks, 0, "nothing replayed on first run");

    // ---- Second run: replay from the recorded journal. ----
    let replay_sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&live_calls),
        })
        .with_replay_journal(first.journal)
        .build()
        .expect("build replay sandbox");

    let second = replay_sandbox.execute_with_journal(TWO_CALL_SCRIPT).await;
    let output = second.result.expect("second run succeeds");

    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        2,
        "no additional live calls — both were replayed"
    );
    assert_eq!(second.replayed_callbacks, 2, "both callbacks replayed");
    assert_eq!(second.journal.entries.len(), 2);
    // The replayed values are the cached ones from the first run (live_call 1 and 2).
    assert!(
        output.stdout.contains("a=1 b=2"),
        "replayed cached values, got: {}",
        output.stdout
    );
}

/// Changing the script so the second callback diverges falls back to live mode
/// from the point of divergence; the matching prefix is still replayed.
#[tokio::test]
async fn replay_falls_back_to_live_on_divergence() {
    let live_calls = Arc::new(AtomicU32::new(0));

    let sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&live_calls),
        })
        .build()
        .expect("build sandbox");

    let first = sandbox.execute_with_journal(TWO_CALL_SCRIPT).await;
    first.result.expect("first run succeeds");
    assert_eq!(live_calls.load(Ordering::SeqCst), 2);

    // Second run: first call matches (step="one"), second diverges (step="changed").
    let divergent_script = r#"
a = await tick(step="one")
b = await tick(step="changed")
print(f"a={a['live_call']} b={b['live_call']}")
"#;

    let replay_sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&live_calls),
        })
        .with_replay_journal(first.journal)
        .build()
        .expect("build replay sandbox");

    let second = replay_sandbox.execute_with_journal(divergent_script).await;
    let output = second.result.expect("second run succeeds");

    assert_eq!(second.replayed_callbacks, 1, "only the prefix replayed");
    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        3,
        "one new live call for the divergent callback"
    );
    // a is replayed (cached live_call=1); b is a fresh live call (3rd overall).
    assert!(output.stdout.contains("a=1 b=3"), "got: {}", output.stdout);
}

/// `execute_with_journal` works without a configured previous journal: it simply
/// records a fresh journal and replays nothing.
#[tokio::test]
async fn journaling_without_previous_records_fresh() {
    let live_calls = Arc::new(AtomicU32::new(0));
    let sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&live_calls),
        })
        .build()
        .expect("build sandbox");

    let outcome = sandbox.execute_with_journal(TWO_CALL_SCRIPT).await;
    outcome.result.expect("run succeeds");
    assert_eq!(outcome.replayed_callbacks, 0);
    assert_eq!(outcome.journal.entries.len(), 2);
    assert_eq!(outcome.journal.code, TWO_CALL_SCRIPT);
}

/// Regression test for concurrent replay: two callbacks launched together via
/// `asyncio.gather` complete in a different order than they were initiated
/// (`slow_a` sleeps longer than `slow_b`). Keyed `(name, args)` matching must
/// replay BOTH on the second run regardless of completion order — under the old
/// positional-cursor model the out-of-order completion silently fell back to
/// live.
const GATHER_SCRIPT: &str = r#"
import asyncio
a, b = await asyncio.gather(slow_a(), slow_b())
print(f"a={a['from']} b={b['from']}")
"#;

#[tokio::test]
async fn concurrent_gather_callbacks_replay_regardless_of_order() {
    let a_calls = Arc::new(AtomicU32::new(0));
    let b_calls = Arc::new(AtomicU32::new(0));

    let build = |a: &Arc<AtomicU32>, b: &Arc<AtomicU32>| {
        sandbox_builder()
            // slow_a finishes AFTER slow_b, so completion order != initiation order.
            .with_callback(SlowCallback {
                name: "slow_a".to_string(),
                delay_ms: 80,
                live_calls: Arc::clone(a),
            })
            .with_callback(SlowCallback {
                name: "slow_b".to_string(),
                delay_ms: 5,
                live_calls: Arc::clone(b),
            })
    };

    // ---- Run 1: record the journal. ----
    let sandbox = build(&a_calls, &b_calls).build().expect("build sandbox");
    let first = sandbox.execute_with_journal(GATHER_SCRIPT).await;
    let first_out = first.result.expect("first run succeeds");
    assert!(
        first_out.stdout.contains("a=slow_a b=slow_b"),
        "got: {}",
        first_out.stdout
    );
    assert_eq!(a_calls.load(Ordering::SeqCst), 1, "slow_a ran live once");
    assert_eq!(b_calls.load(Ordering::SeqCst), 1, "slow_b ran live once");
    assert_eq!(first.journal.entries.len(), 2, "both callbacks journaled");
    assert_eq!(first.replayed_callbacks, 0);

    // ---- Run 2: replay from the recorded journal. ----
    let replay_sandbox = build(&a_calls, &b_calls)
        .with_replay_journal(first.journal)
        .build()
        .expect("build replay sandbox");
    let second = replay_sandbox.execute_with_journal(GATHER_SCRIPT).await;
    let second_out = second.result.expect("second run succeeds");

    assert_eq!(
        second.replayed_callbacks, 2,
        "both concurrent callbacks replayed"
    );
    assert_eq!(
        a_calls.load(Ordering::SeqCst),
        1,
        "slow_a must not be invoked live again"
    );
    assert_eq!(
        b_calls.load(Ordering::SeqCst),
        1,
        "slow_b must not be invoked live again"
    );
    assert!(
        second_out.stdout.contains("a=slow_a b=slow_b"),
        "replayed output mismatch, got: {}",
        second_out.stdout
    );
}
