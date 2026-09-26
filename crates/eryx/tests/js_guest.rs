//! SPIKE: end-to-end tests for the QuickJS guest (`eryx-js-wasm-runtime`).
//!
//! These run JavaScript through the *unmodified* eryx host, which proves the
//! host and WIT contract are language-agnostic. The component is only built on
//! request, so every test is a no-op unless it exists (CI builds it and sets
//! `ERYX_REQUIRE_JS_RUNTIME=1`, which turns a missing component into a failure):
//!
//! ```sh
//! mise run build-eryx-js-runtime   # writes crates/eryx-runtime/runtime-js.wasm
//! cargo nextest run -p eryx --test js_guest
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use eryx::{
    Callback, CallbackError, PythonExecutor, ResourceLimits, Sandbox, SandboxBuilder, Schema,
    SessionExecutor, state,
};
use serde_json::{Value, json};

fn js_runtime_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eryx-runtime/runtime-js.wasm");
    if path.exists() {
        Some(path)
    } else if std::env::var_os("ERYX_REQUIRE_JS_RUNTIME").is_some() {
        panic!(
            "{} not built, but ERYX_REQUIRE_JS_RUNTIME is set",
            path.display()
        );
    } else {
        eprintln!("skipping: {} not built", path.display());
        None
    }
}

/// The builder's typestate requires a "stdlib" directory; the JS guest never
/// reads it, so hand it an empty one.
fn empty_stdlib() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("js-guest-empty-stdlib");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn js_sandbox() -> Option<SandboxBuilder<state::Has, state::Has>> {
    Some(
        Sandbox::builder()
            .with_wasm_file(js_runtime_path()?)
            .with_python_stdlib(empty_stdlib()),
    )
}

async fn js_session() -> Option<SessionExecutor> {
    let executor = PythonExecutor::from_file(js_runtime_path()?)
        .unwrap()
        .with_python_stdlib(empty_stdlib());
    Some(SessionExecutor::new(Arc::new(executor), &[]).await.unwrap())
}

/// A test callback backed by a closure, optionally delayed and counted.
struct TestCallback {
    name: &'static str,
    delay_ms: u64,
    calls: Arc<AtomicU32>,
    handler: fn(Value) -> Result<Value, CallbackError>,
}

impl TestCallback {
    fn new(name: &'static str, handler: fn(Value) -> Result<Value, CallbackError>) -> Self {
        Self {
            name,
            delay_ms: 0,
            calls: Arc::new(AtomicU32::new(0)),
            handler,
        }
    }
}

impl Callback for TestCallback {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "test callback"
    }
    fn parameters_schema(&self) -> Schema {
        Schema::empty()
    }
    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let delay = Duration::from_millis(self.delay_ms);
        let handler = self.handler;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            handler(args)
        })
    }
}

fn add(args: Value) -> Result<Value, CallbackError> {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(json!({ "sum": a + b }))
}

#[tokio::test]
async fn console_output_and_result_variable() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(
            r#"
console.log("hi", 1, {a: 1}, [true, null]);
console.error("oops");
let result = {sum: 1 + 2};
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "hi 1 {\"a\":1} [true,null]\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    assert_eq!(out.result.as_deref(), Some("{\"sum\":3}"));
}

#[tokio::test]
async fn await_callback_by_name_and_via_invoke() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
const x = await add({a: 2, b: 3});
const y = await invoke("add", {a: x.sum, b: 10});
console.log(x.sum, y.sum, JSON.stringify(listCallbacks().map(cb => cb.name)));
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "5 15 [\"add\"]\n");
}

#[tokio::test]
async fn parallel_callbacks_complete_out_of_order() {
    let Some(builder) = js_sandbox() else { return };
    let mut slow = TestCallback::new("slow", |_| Ok(json!("slow")));
    slow.delay_ms = 100;
    let mut fast = TestCallback::new("fast", |_| Ok(json!("fast")));
    fast.delay_ms = 10;
    let sandbox = builder
        .with_callback(slow)
        .with_callback(fast)
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
const order = [];
const track = (p) => p.then((v) => { order.push(v); return v; });
const all = await Promise.all([track(slow()), track(fast()), track(slow()), track(fast())]);
console.log(all.join(","), order.join(","));
"#,
        )
        .await
        .unwrap();

    assert_eq!(
        out.stdout_utf8().unwrap(),
        "slow,fast,slow,fast fast,fast,slow,slow\n"
    );
}

#[tokio::test]
async fn callback_error_rejects_the_promise() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
try {
    await fail();
    console.log("unreachable");
} catch (e) {
    console.log("caught:", e instanceof Error, e.message.includes("nope"));
}
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "caught: true true\n");
}

#[tokio::test]
async fn uncaught_errors_are_guest_exceptions() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .build()
        .unwrap();

    for (code, expected) in [
        // Thrown synchronously, before any await.
        ("throw new TypeError('boom')", "TypeError: boom"),
        // Rejected after the script suspended on a callback.
        (
            "await add({a: 1, b: 2}); throw new RangeError('late')",
            "RangeError: late",
        ),
        // Syntax errors are thrown by the compiler, not the promise.
        ("let = = 1", "SyntaxError"),
        // Throwing a non-Error value.
        ("throw 42", "Uncaught 42"),
    ] {
        let err = sandbox.execute(code).await.unwrap_err();
        let eryx::Error::PythonException(msg) = &err else {
            panic!("{code}: expected a guest exception, got {err:?}");
        };
        assert!(msg.contains(expected), "{code}: {msg}");
    }
}

#[tokio::test]
async fn awaiting_a_promise_that_never_settles_is_an_error() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder.build().unwrap();

    let err = sandbox
        .execute("await new Promise(() => {})")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("never completed"), "{err}");
}

#[tokio::test]
async fn session_state_persists_across_executes() {
    let Some(mut session) = js_session().await else {
        return;
    };

    session
        .execute("var a = 1; let b = 2; const c = 3; function f() { return a + b + c; }")
        .run()
        .await
        .unwrap();
    let out = session
        .execute("b += 10; console.log(f())")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"16\n");

    // The result variable is consumed per execution.
    let out = session.execute("var result = 'first'").run().await.unwrap();
    assert_eq!(out.result.as_deref(), Some("\"first\""));
    let out = session.execute("1 + 1").run().await.unwrap();
    assert_eq!(out.result, None);
}

#[tokio::test]
async fn fuel_limit_stops_an_infinite_loop() {
    let Some(mut session) = js_session().await else {
        return;
    };

    let err = session
        .execute("while (true) {}")
        .with_fuel_limit(50_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err, eryx::Error::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );
}

#[tokio::test]
async fn suspending_callback_halts_the_guest() {
    let Some(builder) = js_sandbox() else { return };
    let approve = TestCallback::new("approve", |_| {
        Err(CallbackError::Suspend("needs human approval".to_string()))
    });
    let marker = TestCallback::new("marker", |_| Ok(json!(null)));
    let marker_calls = Arc::clone(&marker.calls);
    let sandbox = builder
        .with_callback(approve)
        .with_callback(marker)
        .build()
        .unwrap();

    let outcome = sandbox
        .execute_with_journal("try { await approve() } catch {} await marker()")
        .await;

    assert_eq!(marker_calls.load(Ordering::SeqCst), 0);
    let suspended = outcome.suspended.expect("execution should suspend");
    assert_eq!(suspended.name, "approve");
    assert!(matches!(
        outcome.result.unwrap_err(),
        eryx::Error::Suspended(_)
    ));
}

#[tokio::test]
async fn trivial_execute_fuel_cost() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_resource_limits(ResourceLimits::default())
        .build()
        .unwrap();

    let out = sandbox.execute("1").await.unwrap();
    // Not an assertion target, just a number for the spike write-up.
    eprintln!("JS trivial execute fuel: {:?}", out.stats.fuel_consumed);
    assert!(out.stats.fuel_consumed.is_some());
}
