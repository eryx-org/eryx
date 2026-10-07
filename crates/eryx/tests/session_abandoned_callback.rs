//! Regression tests: a persistent session must not hang when an execution
//! ends with a host callback still in flight.
//!
//! A session's store outlives each execution, so the host future for an
//! abandoned callback stays parked in it. It used to hold a clone of the
//! callback channel's sender, which kept the channel open, so the caller's
//! wait on the callback handler never finished.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use eryx::{
    Callback, CallbackError, InProcessSession, JsonSchema, PythonExecutor, ResourceLimits, Sandbox,
    Session, SessionExecutor, TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Upper bound for an execution that abandons a `SLOW_MS` callback. Before the
/// fix these executions never returned.
const HANG: Duration = Duration::from_secs(10);

#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Milliseconds to sleep
    ms: u64,
}

/// Sleeps on the host for the requested time.
struct Slow;

impl TypedCallback for Slow {
    type Args = SleepArgs;

    fn name(&self) -> &str {
        "slow"
    }

    fn description(&self) -> &str {
        "Sleeps for the given milliseconds"
    }

    fn invoke_typed(
        &self,
        args: SleepArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(args.ms)).await;
            Ok(json!({"slept": args.ms}))
        })
    }
}

/// One callback is still awaiting the host when the script fails.
const GATHER_FAST_FAILURE: &str = r#"
import asyncio
async def boom():
    raise ValueError("fast failure")
await asyncio.gather(slow(ms=300), boom())
"#;

/// The script finishes without awaiting a callback task.
const CREATE_TASK: &str = r#"
import asyncio
t = asyncio.create_task(slow(ms=300))
await asyncio.sleep(0)
"#;

/// The script cancels a callback task that is awaiting the host.
const CANCEL: &str = r#"
import asyncio
t = asyncio.create_task(slow(ms=300))
await asyncio.sleep(0)
t.cancel()
try:
    await t
except asyncio.CancelledError:
    pass
"#;

fn sandbox(limits: ResourceLimits) -> Sandbox {
    Sandbox::builder()
        .with_embedded_runtime()
        .with_callback(Slow)
        .with_resource_limits(limits)
        .build()
        .unwrap()
}

async fn execute_bounded(
    session: &mut InProcessSession<'_>,
    code: &str,
) -> Result<eryx::ExecuteResult, eryx::Error> {
    tokio::time::timeout(HANG, session.execute(code))
        .await
        .expect("session execute hung with a callback in flight")
}

#[tokio::test]
async fn session_survives_gather_with_fast_failure() {
    let sandbox = sandbox(ResourceLimits::default());
    let mut session = InProcessSession::new(&sandbox).await.unwrap();

    let error = execute_bounded(&mut session, GATHER_FAST_FAILURE)
        .await
        .expect_err("gather should propagate the ValueError");
    assert!(
        error.to_string().contains("fast failure"),
        "unexpected error: {error}"
    );

    // The session stays usable and callbacks still route to the new run.
    let output = execute_bounded(&mut session, "print(await slow(ms=10))")
        .await
        .expect("follow-up execute");
    assert_eq!(output.stdout_text().trim(), "{'slept': 10}");
}

#[tokio::test]
async fn session_gather_with_execution_timeout_returns() {
    // The hang happened after the guest returned, so execution_timeout never
    // rescued it. Make sure the combination returns too.
    let limits = ResourceLimits::default().with_execution_timeout(Duration::from_secs(5));
    let sandbox = sandbox(limits);
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    let error = execute_bounded(&mut session, GATHER_FAST_FAILURE)
        .await
        .expect_err("gather should propagate the ValueError");
    assert!(
        error.to_string().contains("fast failure"),
        "unexpected error: {error}"
    );
}

// The two tests below only check that the execution returns. Until the guest
// cancels outstanding subtasks when `execute` finishes, it traps with
// "resource has children" in these cases and poisons the session, so neither
// the result nor a follow-up execute is asserted here.

#[tokio::test]
async fn session_returns_when_callback_task_is_never_awaited() {
    let sandbox = sandbox(ResourceLimits::default());
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    let _ = execute_bounded(&mut session, CREATE_TASK).await;
}

#[tokio::test]
async fn session_returns_when_callback_task_is_cancelled() {
    let sandbox = sandbox(ResourceLimits::default());
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    let _ = execute_bounded(&mut session, CANCEL).await;
}

/// The gRPC server and the Python `Session` drive a `SessionExecutor` with
/// their own `run_callback_handler` task and wait for it after `run()`. That
/// handler must finish once `run()` returns.
#[tokio::test]
async fn session_executor_callback_handler_finishes_after_run() {
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    #[allow(unsafe_code)]
    let executor = Arc::new(
        unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
            .unwrap()
            .with_python_stdlib(resources.stdlib()),
    );
    let callback: Arc<dyn Callback> = Arc::new(Slow);
    let callbacks = vec![Arc::clone(&callback)];
    let mut session = SessionExecutor::new(executor, &callbacks).await.unwrap();

    let (callback_tx, callback_rx) = tokio::sync::mpsc::channel(32);
    let handler = tokio::spawn(eryx::callback_handler::run_callback_handler(
        callback_rx,
        Arc::new(HashMap::from([("slow".to_string(), callback)])),
        ResourceLimits::default(),
        Arc::new(HashMap::new()),
    ));
    let result = session
        .execute(GATHER_FAST_FAILURE)
        .with_callbacks(&callbacks, callback_tx)
        .run()
        .await;
    assert!(result.is_err(), "gather should propagate the ValueError");

    let invocations = tokio::time::timeout(HANG, handler)
        .await
        .expect("callback handler never finished after run()")
        .unwrap();
    assert_eq!(invocations, 1);
}
