//! Limits and output handling for sessions, sandboxes and pools.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eryx::{Error, PythonExecutor, ResourceLimits, Session as _, SessionExecutor};

static EXECUTOR: OnceLock<Arc<PythonExecutor>> = OnceLock::new();

fn executor() -> Arc<PythonExecutor> {
    EXECUTOR
        .get_or_init(|| {
            let resources = eryx::embedded::EmbeddedResources::get().unwrap();
            #[allow(unsafe_code)]
            Arc::new(
                unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
                    .unwrap()
                    .with_python_stdlib(resources.stdlib()),
            )
        })
        .clone()
}

async fn session_with_timeout(timeout: Duration) -> SessionExecutor {
    let limits = ResourceLimits::default().with_execution_timeout(timeout);
    SessionExecutor::new_with_limits(executor(), &[], &limits)
        .await
        .expect("session construction")
}

/// Guard against the test itself hanging if a limit isn't applied.
async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("operation should be bounded by the session timeout")
}

#[tokio::test]
async fn snapshot_state_is_bounded_by_execution_timeout() {
    let mut session = session_with_timeout(Duration::from_millis(500)).await;
    session
        .execute(
            "class Spin:\n    def __reduce__(self):\n        while True:\n            pass\nspin = Spin()",
        )
        .run()
        .await
        .expect("setup");

    let err = bounded(session.snapshot_state())
        .await
        .expect_err("snapshot should time out");
    assert!(matches!(err, Error::Timeout(_)), "got {err:?}");

    // The session is usable again, from a fresh state.
    let output = bounded(session.execute("print('spin' in globals())").run())
        .await
        .expect("session recovers");
    assert_eq!(output.stdout_text().trim(), "False");
    assert!(output.state_was_reset);
}

#[tokio::test]
async fn clear_state_is_bounded_by_execution_timeout() {
    let mut session = session_with_timeout(Duration::from_millis(500)).await;
    session
        .execute(
            "class Spin:\n    def __del__(self):\n        while True:\n            pass\nspin = Spin()",
        )
        .run()
        .await
        .expect("setup");

    let err = bounded(session.clear_state())
        .await
        .expect_err("clear should time out");
    assert!(matches!(err, Error::Timeout(_)), "got {err:?}");

    bounded(session.execute("x = 1").run())
        .await
        .expect("session recovers");
}

const SPIN_ON_SNAPSHOT: &str =
    "class Spin:\n    def __reduce__(self):\n        while True:\n            pass\nspin = Spin()";

#[tokio::test]
async fn state_op_timeout_can_be_set_separately() {
    let mut session = SessionExecutor::new(executor(), &[])
        .await
        .expect("session construction");
    assert_eq!(session.state_op_timeout(), None);
    session.set_execution_timeout(Some(Duration::from_millis(100)));
    assert_eq!(
        session.state_op_timeout(),
        Some(eryx::session::MIN_STATE_OP_TIMEOUT)
    );
    session.set_execution_timeout(None);
    session.set_state_op_timeout(Some(Duration::from_millis(300)));
    session
        .execute(SPIN_ON_SNAPSHOT)
        .run()
        .await
        .expect("setup");

    let err = bounded(session.snapshot_state())
        .await
        .expect_err("snapshot should time out");
    assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
}

#[tokio::test]
async fn state_ops_are_bounded_by_fuel() {
    let limits = ResourceLimits::unlimited().with_max_fuel(1_000);
    let mut session = SessionExecutor::new_with_limits(executor(), &[], &limits)
        .await
        .expect("session construction");
    session.set_fuel_limit(None);
    session
        .execute(SPIN_ON_SNAPSHOT)
        .run()
        .await
        .expect("setup");
    session.set_fuel_limit(Some(1_000));

    let err = tokio::time::timeout(Duration::from_secs(120), session.snapshot_state())
        .await
        .expect("bounded by fuel")
        .expect_err("snapshot should run out of fuel");
    assert!(matches!(err, Error::FuelExhausted { .. }), "got {err:?}");
}

const FD_WRITES: &str = "import os, sys\nos.write(1, b'fd-out\\n')\nos.write(2, b'fd-err\\n')\nsys.__stdout__.write('dunder-out\\n')\nsys.__stdout__.flush()";

fn assert_fd_writes_captured(stdout: &[u8], stderr: &[u8]) {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    assert!(stdout.contains("fd-out"), "stdout: {stdout:?}");
    assert!(stdout.contains("dunder-out"), "stdout: {stdout:?}");
    assert!(stderr.contains("fd-err"), "stderr: {stderr:?}");
}

#[tokio::test]
async fn sandbox_captures_direct_fd_writes() {
    let sandbox = eryx::Sandbox::embedded().build().expect("sandbox");
    let output = sandbox.execute(FD_WRITES).await.expect("execute");
    assert_fd_writes_captured(&output.stdout, &output.stderr);
}

#[tokio::test]
async fn session_captures_direct_fd_writes_per_execution() {
    let mut session = session_with_timeout(Duration::from_secs(10)).await;
    let output = session.execute(FD_WRITES).run().await.expect("execute");
    assert_fd_writes_captured(&output.stdout, &output.stderr);

    // Each execution only reports its own writes.
    let output = session
        .execute("print('next')")
        .run()
        .await
        .expect("execute");
    assert_eq!(output.stdout_text().trim(), "next");
}

#[tokio::test]
async fn pooled_sandbox_keeps_builder_limits_across_leases() {
    let limits = ResourceLimits::default()
        .with_execution_timeout(Duration::from_millis(1234))
        .with_max_fuel(5_000_000_000);
    let built = limits.clone();
    let pool = eryx::SandboxPool::with_builder(
        move || {
            eryx::Sandbox::embedded()
                .with_resource_limits(limits.clone())
                .build()
        },
        eryx::PoolConfig {
            max_size: 1,
            min_idle: 1,
            ..Default::default()
        },
    )
    .await
    .expect("pool");

    for _ in 0..2 {
        let lease = pool.acquire().await.expect("acquire");
        assert_eq!(lease.resource_limits(), &built);
        // A per-request override lasts only for this lease.
        let lease = lease.with_resource_limits(ResourceLimits::unlimited());
        assert_eq!(lease.resource_limits(), &ResourceLimits::unlimited());
    }
}

fn sandbox_with_secret() -> eryx::Sandbox {
    eryx::Sandbox::embedded()
        .with_secret("API_KEY", "real-value", vec!["example.com".to_string()])
        .build()
        .expect("sandbox")
}

fn assert_placeholder_redacted(text: &str) {
    assert!(!text.contains("ERYX_SECRET_PLACEHOLDER"), "got {text:?}");
    assert!(text.contains("[REDACTED]"), "got {text:?}");
}

#[tokio::test]
async fn python_exception_text_is_redacted() {
    let sandbox = sandbox_with_secret();
    let code = "import os\nraise RuntimeError(os.environ['API_KEY'])";

    let err = sandbox.execute(code).await.expect_err("raises");
    let Error::PythonException(traceback) = err else {
        panic!("expected a Python exception, got {err:?}");
    };
    assert_placeholder_redacted(&traceback);

    let mut session = eryx::InProcessSession::new(&sandbox)
        .await
        .expect("session");
    let err = session.execute(code).await.expect_err("raises");
    let Error::PythonException(traceback) = err else {
        panic!("expected a Python exception, got {err:?}");
    };
    assert_placeholder_redacted(&traceback);
}

#[tokio::test]
async fn in_process_session_output_is_redacted() {
    let sandbox = sandbox_with_secret();
    let mut session = eryx::InProcessSession::new(&sandbox)
        .await
        .expect("session");
    let result = session
        .execute(
            "import os, sys\nprint(os.environ['API_KEY'])\nsys.stderr.write(os.environ['API_KEY'])",
        )
        .await
        .expect("execute");
    assert_placeholder_redacted(&String::from_utf8_lossy(&result.stdout));
    assert_placeholder_redacted(&String::from_utf8_lossy(&result.stderr));
}

#[tokio::test]
async fn trace_collection_is_off_by_default() {
    let sandbox = eryx::Sandbox::embedded().build().expect("sandbox");
    let result = sandbox.execute("x = 1\ny = 2").await.expect("execute");
    assert!(result.trace.is_empty(), "got {:?}", result.trace);

    let sandbox = eryx::Sandbox::embedded()
        .with_trace_collection(true)
        .build()
        .expect("sandbox");
    let result = sandbox.execute("x = 1\ny = 2").await.expect("execute");
    assert!(!result.trace.is_empty());
}

/// Counts bytes delivered to the output handler, slowly.
struct SlowCounter(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl eryx::OutputHandler for SlowCounter {
    async fn on_output(&self, chunk: &[u8]) {
        tokio::time::sleep(Duration::from_micros(10)).await;
        self.0
            .fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn streamed_output_is_capped_per_execution() {
    let streamed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sandbox = eryx::Sandbox::embedded()
        .with_output_handler(SlowCounter(Arc::clone(&streamed)))
        .build()
        .expect("sandbox");
    // 24 MiB in 1 MiB writes, past the 16 MiB stream limit.
    let result = sandbox
        .execute(
            "import sys\nchunk = 'x' * (1 << 20)\nfor _ in range(24):\n    sys.stdout.write(chunk)",
        )
        .await
        .expect("execute");
    assert_eq!(result.stdout.len(), 24 << 20, "captured stdout is complete");
    let streamed = streamed.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        streamed <= 16 << 20 && streamed > 0,
        "streamed {streamed} bytes"
    );
}
