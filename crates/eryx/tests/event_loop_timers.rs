//! Integration tests for event-loop timers (`asyncio.sleep`, `wait_for`,
//! `asyncio.timeout`), which are backed by the host `sleep` import.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eryx::{CallbackError, Error, ResourceLimits, Sandbox, TypedCallback};
use serde_json::{Value, json};

#[cfg(not(feature = "embedded"))]
fn crates_dir() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf()
}

fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib = std::env::var("ERYX_PYTHON_STDLIB").map_or_else(
            |_| crates_dir().join("eryx-wasm-runtime/tests/python-stdlib"),
            PathBuf::from,
        );
        Sandbox::builder()
            .with_wasm_file(crates_dir().join("eryx-runtime/runtime.wasm"))
            .with_python_stdlib(&stdlib)
    }
}

fn sandbox() -> Sandbox {
    sandbox_builder().build().expect("Failed to build sandbox")
}

async fn run(code: &str) -> String {
    let result = sandbox().execute(code).await;
    result.expect("execution failed").stdout_text()
}

/// A callback that takes a while, to overlap with timers. Counts completed runs.
#[derive(Clone, Default)]
struct SlowCallback(Arc<AtomicU32>);

impl TypedCallback for SlowCallback {
    type Args = ();

    fn name(&self) -> &str {
        "slow"
    }

    fn description(&self) -> &str {
        "Returns after 100ms"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!("done"))
        })
    }
}

#[tokio::test]
async fn sleep_waits_at_least_the_delay() {
    let out = run(r#"
import asyncio
loop = asyncio.get_running_loop()
t0 = loop.time()
await asyncio.sleep(0.1)
elapsed = loop.time() - t0
print(elapsed >= 0.1, elapsed < 1.0)
"#)
    .await;
    assert_eq!(out, "True True\n");
}

#[tokio::test]
async fn sleep_zero_and_result_value() {
    let out = run(r#"
import asyncio
print(await asyncio.sleep(0, "zero"), await asyncio.sleep(0.01, "short"))
"#)
    .await;
    assert_eq!(out, "zero short\n");
}

#[tokio::test]
async fn loop_time_is_monotonic() {
    let out = run(r#"
import asyncio
loop = asyncio.get_running_loop()
a = loop.time(); b = loop.time()
print(isinstance(a, float), b >= a)
"#)
    .await;
    assert_eq!(out, "True True\n");
}

#[tokio::test]
async fn gathered_sleeps_run_concurrently() {
    let out = run(r#"
import asyncio
loop = asyncio.get_running_loop()
t0 = loop.time()
print(await asyncio.gather(asyncio.sleep(0.2, 1), asyncio.sleep(0.2, 2), asyncio.sleep(0.2, 3)))
print(loop.time() - t0 < 0.5)
"#)
    .await;
    assert_eq!(out, "[1, 2, 3]\nTrue\n");
}

#[tokio::test]
async fn timers_fire_in_deadline_order() {
    let out = run(r#"
import asyncio
order = []
loop = asyncio.get_running_loop()
done = loop.create_future()
loop.call_later(0.06, order.append, 3)
loop.call_later(0.02, order.append, 1)
loop.call_at(loop.time() + 0.04, order.append, 2)
loop.call_later(0.08, done.set_result, None)
await done
print(order)
"#)
    .await;
    assert_eq!(out, "[1, 2, 3]\n");
}

#[tokio::test]
async fn wait_for_times_out() {
    let start = Instant::now();
    let out = run(r#"
import asyncio
try:
    await asyncio.wait_for(asyncio.sleep(30), timeout=0.05)
except TimeoutError:
    print("timed out")
"#)
    .await;
    assert_eq!(out, "timed out\n");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn asyncio_timeout_context_manager() {
    let out = run(r#"
import asyncio
try:
    async with asyncio.timeout(0.05):
        await asyncio.sleep(30)
except TimeoutError:
    print("timed out")
async with asyncio.timeout(5):
    await asyncio.sleep(0.01)
print("ok")
"#)
    .await;
    assert_eq!(out, "timed out\nok\n");
}

/// A `wait_for` that completes cancels its timeout timer, so the execution
/// does not wait for (or leak) the host sleep.
#[tokio::test]
async fn completed_wait_for_cancels_its_timer() {
    let start = Instant::now();
    let out = run(r#"
import asyncio
print(await asyncio.wait_for(asyncio.sleep(0, "fast"), timeout=30))
h = asyncio.get_running_loop().call_later(30, print, "never")
h.cancel()
h.cancel()
"#)
    .await;
    assert_eq!(out, "fast\n");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

/// A timer still pending when the script finishes is dropped with the
/// execution rather than keeping it alive.
#[tokio::test]
async fn pending_timer_does_not_outlive_execution() {
    let start = Instant::now();
    let out = run(r#"
import asyncio
asyncio.get_running_loop().call_later(30, print, "never")
asyncio.ensure_future(asyncio.sleep(30))
await asyncio.sleep(0)
print("end")
"#)
    .await;
    assert_eq!(out, "end\n");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn timer_and_callback_overlap() {
    let sandbox = sandbox_builder()
        .with_callback(SlowCallback::default())
        .build()
        .expect("Failed to build sandbox");
    let out = sandbox
        .execute(
            r#"
import asyncio
loop = asyncio.get_running_loop()
t0 = loop.time()
r = await asyncio.gather(slow(), asyncio.sleep(0.1, "slept"))
print(r, loop.time() - t0 < 0.5)
"#,
        )
        .await
        .expect("execution failed")
        .stdout_text();
    assert_eq!(out, "['done', 'slept'] True\n");
}

#[tokio::test]
async fn wait_for_bounds_a_callback() {
    let sandbox = sandbox_builder()
        .with_callback(SlowCallback::default())
        .build()
        .expect("Failed to build sandbox");
    let out = sandbox
        .execute(
            r#"
import asyncio
try:
    await asyncio.wait_for(slow(), timeout=0.01)
except TimeoutError:
    print("timed out")
print(await asyncio.wait_for(slow(), timeout=5))
"#,
        )
        .await
        .expect("execution failed")
        .stdout_text();
    assert_eq!(out, "timed out\ndone\n");
}

#[tokio::test]
async fn execution_timeout_wins_over_sleep() {
    let sandbox = sandbox_builder()
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(500)),
        )
        .build()
        .expect("Failed to build sandbox");
    let start = Instant::now();
    let result = sandbox
        .execute("import asyncio\nawait asyncio.sleep(60)")
        .await;
    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

/// Sleeping suspends the guest, so a long sleep costs no more fuel than a short one.
#[tokio::test]
async fn sleeping_burns_no_fuel() {
    let sandbox = sandbox();
    let fuel = async |delay: &str| {
        let code = format!("import asyncio\nawait asyncio.sleep({delay})");
        let result = sandbox.execute(&code).await.expect("execution failed");
        result.stats.fuel_consumed.expect("fuel tracked")
    };
    let short = fuel("0.001").await;
    let long = fuel("0.3").await;
    assert!(long < short * 2, "short={short} long={long}");
}

/// A `wait_for` timeout only stops Python waiting: the host callback still
/// completes and is journaled, so on replay it returns instantly from cache
/// and the same `wait_for` no longer times out.
#[tokio::test]
async fn timed_out_callback_is_still_journaled() {
    let code = r#"
import asyncio
try:
    print(await asyncio.wait_for(slow(), timeout=0.01))
except TimeoutError:
    print("timed out")
"#;
    let slow = SlowCallback::default();
    let build = || sandbox_builder().with_callback(slow.clone());

    let first = build().build().unwrap().execute_with_journal(code).await;
    assert_eq!(first.result.unwrap().stdout_text(), "timed out\n");
    assert_eq!(slow.0.load(Ordering::SeqCst), 1);
    assert!(
        first
            .journal
            .entries
            .iter()
            .any(|e| e.name == "slow" && e.result == Ok(json!("done"))),
        "journal: {:?}",
        first.journal.entries
    );

    let replay = build()
        .with_replay_journal(first.journal)
        .build()
        .unwrap()
        .execute_with_journal(code)
        .await;
    assert_eq!(replay.result.unwrap().stdout_text(), "done\n");
    assert_eq!(
        slow.0.load(Ordering::SeqCst),
        1,
        "replay re-ran the callback"
    );
}
