//! End-to-end integration tests for callback-result replay.
//!
//! These exercise [`Sandbox::execute_with_journal`] through the full WASM
//! Python runtime: a journal is recorded on a first run and then replayed on a
//! second run, proving that matching callbacks are served from cache (the real
//! callback is not invoked) and that suspension surfaces correctly.
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

/// A callback that always requests suspension with a fixed reason.
struct SuspendingCallback {
    name: String,
    reason: String,
    live_calls: Arc<AtomicU32>,
}

impl Callback for SuspendingCallback {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Always suspends"
    }
    fn parameters_schema(&self) -> Schema {
        Schema::empty()
    }
    fn invoke(
        &self,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        let reason = self.reason.clone();
        Box::pin(async move { Err(CallbackError::Suspend(reason)) })
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
    assert!(first.suspended.is_none());

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

/// A suspending callback stops the run, surfaces the suspension, and still
/// returns the journal of everything that completed before it.
#[tokio::test]
async fn suspension_surfaces_and_preserves_prefix_journal() {
    let tick_calls = Arc::new(AtomicU32::new(0));
    let approve_calls = Arc::new(AtomicU32::new(0));

    let sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&tick_calls),
        })
        .with_callback(SuspendingCallback {
            name: "approve".to_string(),
            reason: "needs human approval".to_string(),
            live_calls: Arc::clone(&approve_calls),
        })
        .build()
        .expect("build sandbox");

    let script = r#"
a = await tick(step="one")
b = await approve(amount=100)
print("should not reach here")
"#;

    let outcome = sandbox.execute_with_journal(script).await;

    // The suspending callback ran live and was recorded as a suspension.
    let suspended = outcome.suspended.expect("should be suspended");
    assert_eq!(suspended.name, "approve");
    assert_eq!(suspended.reason, "needs human approval");
    assert_eq!(approve_calls.load(Ordering::SeqCst), 1);

    // The completed `tick` callback is journaled; the suspended one is not.
    assert_eq!(
        outcome.journal.entries.len(),
        1,
        "only the prefix journaled"
    );
    assert_eq!(outcome.journal.entries[0].name, "tick");

    // Re-run with the journal: `tick` replays (no new live call) and `approve`
    // runs live again (suspends again).
    let resume_sandbox = sandbox_builder()
        .with_callback(CountingCallback {
            name: "tick".to_string(),
            live_calls: Arc::clone(&tick_calls),
        })
        .with_callback(SuspendingCallback {
            name: "approve".to_string(),
            reason: "needs human approval".to_string(),
            live_calls: Arc::clone(&approve_calls),
        })
        .with_replay_journal(outcome.journal)
        .build()
        .expect("build resume sandbox");

    let resumed = resume_sandbox.execute_with_journal(script).await;
    assert_eq!(resumed.replayed_callbacks, 1, "tick replayed on resume");
    assert_eq!(
        tick_calls.load(Ordering::SeqCst),
        1,
        "tick not invoked live a second time"
    );
    assert!(resumed.suspended.is_some(), "still suspended at approve");
    assert_eq!(
        approve_calls.load(Ordering::SeqCst),
        2,
        "approve ran live again"
    );
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
