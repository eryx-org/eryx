//! Regression coverage for executions that end with a host callback still in
//! flight. The guest must cancel and release such subtasks: leaving them joined
//! to the waitable set trapped ("resource has children"), and on the exception
//! path they leaked into the next execution of a persistent session.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use eryx::{CallbackError, Error, InProcessSession, JsonSchema, Sandbox, Session, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Milliseconds to sleep
    ms: u64,
}

struct Slow;

impl TypedCallback for Slow {
    type Args = SleepArgs;
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "sleeps on the host"
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

fn sandbox() -> Sandbox {
    Sandbox::builder()
        .with_embedded_runtime()
        .with_callback(Slow)
        .build()
        .unwrap()
}

/// A task left running when the script ends.
const NEVER_AWAITED: &str = r#"
import asyncio
t = asyncio.create_task(slow(ms=300))
await asyncio.sleep(0)
print("script end")
"#;

/// A pending callback cancelled from Python; later callbacks still work.
const CANCELLED: &str = r#"
import asyncio
t = asyncio.create_task(slow(ms=300))
await asyncio.sleep(0)
t.cancel()
try:
    await t
except asyncio.CancelledError:
    print("cancelled")
print(await slow(ms=1))
"#;

/// A callback abandoned because a `gather` sibling raised.
const GATHER_FAILURE: &str = r#"
import asyncio
async def boom():
    raise ValueError("fast failure")
await asyncio.gather(slow(ms=300), boom())
"#;

const HANG: Duration = Duration::from_secs(8);

fn assert_gather_failure(r: Result<eryx::ExecuteResult, Error>) {
    match r {
        Err(Error::PythonException(tb)) => assert!(tb.contains("fast failure"), "{tb}"),
        other => panic!("expected the script's ValueError, got {other:?}"),
    }
}

#[tokio::test]
async fn sandbox_never_awaited_task() {
    let out = tokio::time::timeout(HANG, sandbox().execute(NEVER_AWAITED))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.stdout_text(), "script end\n");
}

#[tokio::test]
async fn sandbox_cancelled_task() {
    let out = tokio::time::timeout(HANG, sandbox().execute(CANCELLED))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.stdout_text(), "cancelled\n{'slept': 1}\n");
}

#[tokio::test]
async fn sandbox_gather_failure() {
    let r = tokio::time::timeout(HANG, sandbox().execute(GATHER_FAILURE))
        .await
        .unwrap();
    assert_gather_failure(r);
}

/// Each scenario leaves the session usable: the next execute runs a callback.
#[tokio::test]
async fn session_usable_after_abandoned_callbacks() {
    let sb = sandbox();
    let mut session = InProcessSession::new(&sb).await.unwrap();
    for code in [NEVER_AWAITED, CANCELLED, GATHER_FAILURE] {
        let first = tokio::time::timeout(HANG, session.execute(code))
            .await
            .unwrap();
        if code == GATHER_FAILURE {
            assert_gather_failure(first);
        } else {
            first.unwrap();
        }
        let next = tokio::time::timeout(HANG, session.execute("print(await slow(ms=1))"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.stdout_text(), "{'slept': 1}\n");
    }
}
