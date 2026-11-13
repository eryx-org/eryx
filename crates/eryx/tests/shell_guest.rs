//! SPIKE: end-to-end tests for the brush shell guest (`eryx-shell-wasm-runtime`).
//!
//! These run bash through the *unmodified* eryx host, like `js_guest.rs` on the
//! QuickJS spike. The component is only built on request, so every test is a
//! no-op unless it exists:
//!
//! ```sh
//! (cd crates/eryx-shell-wasm-runtime && cargo build --target wasm32-wasip2 --release)
//! cargo nextest run -p eryx --test shell_guest --no-capture
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use eryx::{
    Callback, CallbackError, ErrorKind, PythonExecutor, Sandbox, SandboxBuilder, Schema,
    SessionExecutor, state,
};
use serde_json::{Value, json};

fn shell_runtime_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../eryx-shell-wasm-runtime/target/wasm32-wasip2/release/eryx_shell_wasm_runtime.wasm",
    );
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: {} not built", path.display());
        None
    }
}

/// The builder's typestate requires a "stdlib" directory; the shell guest
/// never reads it, so hand it an empty one.
fn empty_stdlib() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shell-guest-empty-stdlib");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn shell_sandbox() -> Option<SandboxBuilder<state::Has, state::Has>> {
    Some(
        Sandbox::builder()
            .with_wasm_file(shell_runtime_path()?)
            .with_python_stdlib(empty_stdlib()),
    )
}

fn shell_executor() -> Option<Arc<PythonExecutor>> {
    Some(Arc::new(
        PythonExecutor::from_file(shell_runtime_path()?)
            .unwrap()
            .with_python_stdlib(empty_stdlib()),
    ))
}

struct TestCallback {
    name: &'static str,
    calls: Arc<AtomicU32>,
    handler: fn(Value) -> Result<Value, CallbackError>,
}

impl TestCallback {
    fn new(name: &'static str, handler: fn(Value) -> Result<Value, CallbackError>) -> Self {
        Self {
            name,
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
        let handler = self.handler;
        Box::pin(async move { handler(args) })
    }
}

fn add(args: Value) -> Result<Value, CallbackError> {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(json!(a + b))
}

#[tokio::test]
async fn stdout_stderr_and_result_variable() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(
            r#"
greet() { echo "hello, $1"; }
greet world
echo oops >&2
for i in 1 2 3; do echo -n "$i,"; done; echo
result='{"sum": 3}'
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "hello, world\n1,2,3,\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    assert_eq!(out.result.as_deref(), Some("{\"sum\": 3}"));
}

#[tokio::test]
async fn nonzero_exit_is_a_guest_exception_with_output() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let err = sandbox.execute("echo before; exit 3").await.unwrap_err();
    let ErrorKind::PythonException { traceback, .. } = err.kind() else {
        panic!("expected a guest exception, got {err:?}");
    };
    assert_eq!(traceback, "exit status 3");
    assert_eq!(err.stdout(), Some(&b"before\n"[..]));

    // A missing command is a script failure too. Without a spawn handler
    // brush reports 126 ("not supported"), not bash's 127.
    let err = sandbox.execute("no-such-command").await.unwrap_err();
    assert!(err.to_string().contains("exit status 126"), "{err}");
}

#[tokio::test]
async fn callbacks_are_commands() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
x=$(add --a 2 --b 3)
y=$(invoke add "{\"a\": $x, \"b\": 10}")
z=$(add --json '{"a": 1}' --b 1)
echo "$x $y $z"
if ! fail 2>/dev/null; then echo "fail failed"; fi
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "5 15 2\nfail failed\n");
}

#[tokio::test]
async fn session_state_persists_and_snapshots_round_trip() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(Arc::clone(&executor), &[])
        .await
        .unwrap();

    session
        .execute("count=1; bump() { count=$((count + 1)); }; alias hi='echo hi'")
        .run()
        .await
        .unwrap();
    let out = session
        .execute("bump; bump; echo $count")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"3\n");

    // The result variable is consumed per execution.
    let out = session.execute("result=first").run().await.unwrap();
    assert_eq!(out.result.as_deref(), Some("\"first\""));
    let out = session.execute("true").run().await.unwrap();
    assert_eq!(out.result, None);

    let snapshot = session.snapshot_state().await.unwrap();
    eprintln!("shell snapshot: {} bytes", snapshot.data().len());

    let mut fresh = SessionExecutor::new(executor, &[]).await.unwrap();
    fresh.restore_state(&snapshot).await.unwrap();
    let out = fresh.execute("bump; echo $count").run().await.unwrap();
    assert_eq!(out.stdout, b"4\n");

    fresh.clear_state().await.unwrap();
    let out = fresh.execute("echo \"[${count:-}]\"").run().await.unwrap();
    assert_eq!(out.stdout, b"[]\n");
}

#[tokio::test]
async fn fuel_limit_stops_an_infinite_loop() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();

    let err = session
        .execute("while true; do :; done")
        .with_fuel_limit(50_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );
}

#[tokio::test]
async fn suspending_callback_halts_the_guest() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
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
        .execute_with_journal("approve || true; marker", None)
        .await;

    assert_eq!(marker_calls.load(Ordering::SeqCst), 0);
    let suspended = outcome.suspended.expect("execution should suspend");
    assert_eq!(suspended.name, "approve");
    assert!(matches!(
        outcome.result.unwrap_err().kind(),
        ErrorKind::Suspended(_)
    ));
}

/// Not an assertion target: numbers for the spike write-up. Run in release.
#[tokio::test]
async fn latency_numbers() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    const N: u32 = 50;

    sandbox.execute("echo warm").await.unwrap();
    let start = Instant::now();
    let mut fuel = 0;
    for _ in 0..N {
        fuel = sandbox
            .execute("echo hi")
            .await
            .unwrap()
            .stats
            .fuel_consumed
            .unwrap_or(0);
    }
    eprintln!(
        "stateless `echo hi`: {:?}/exec, {fuel} fuel",
        start.elapsed() / N
    );

    let mut session = SessionExecutor::new(shell_executor().unwrap(), &[])
        .await
        .unwrap();
    session.execute("x=0").run().await.unwrap();
    let start = Instant::now();
    for _ in 0..N {
        session.execute("x=$((x + 1))").run().await.unwrap();
    }
    eprintln!("session `x=$((x+1))`: {:?}/exec", start.elapsed() / N);
}
