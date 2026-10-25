//! Regression tests for guest runtime behaviour: `sys.exit()`, the socket and
//! ssl shims, the streaming stdout/stderr writers, snapshots and callbacks.
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eryx::{
    Callback, CallbackError, Error, InProcessSession, NetConfig, PythonExecutor, Sandbox, Session,
    SessionExecutor, TypedCallback,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn sandbox() -> Sandbox {
    Sandbox::embedded().build().unwrap()
}

fn network_sandbox(connect_timeout: Duration, io_timeout: Duration) -> Sandbox {
    Sandbox::embedded()
        .with_network(
            NetConfig::permissive()
                .with_connect_timeout(connect_timeout)
                .with_io_timeout(io_timeout),
        )
        .build()
        .unwrap()
}

fn assert_script_error(result: Result<eryx::ExecuteResult, Error>, needle: &str) {
    match result {
        Err(e @ Error::PythonException(_)) => {
            assert!(e.to_string().contains(needle), "{needle:?} not in: {e}");
        }
        other => panic!("expected a script exception containing {needle:?}, got {other:?}"),
    }
}

// =============================================================================
// sys.exit()
// =============================================================================

#[tokio::test]
async fn sys_exit_zero_ends_the_script_successfully() {
    let out = sandbox()
        .execute("import sys\nprint('before')\nsys.exit(0)\nprint('after')")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"before\n");
}

#[tokio::test]
async fn sys_exit_nonzero_is_a_script_error_and_the_sandbox_survives() {
    let sandbox = sandbox();
    assert_script_error(sandbox.execute("raise SystemExit(3)").await, "SystemExit");
    let out = sandbox.execute("print('alive')").await.unwrap();
    assert_eq!(out.stdout, b"alive\n");
}

#[tokio::test]
async fn sys_exit_keeps_the_session_alive() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session.execute("x = 1").await.unwrap();
    assert_script_error(
        session.execute("import sys\nsys.exit('bye')").await,
        "SystemExit: bye",
    );
    let out = session.execute("print(x)").await.unwrap();
    assert_eq!(out.stdout, b"1\n");
}

#[tokio::test]
async fn sys_exit_after_await_keeps_the_session_alive() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    // The sleep suspends the execution, so the exit happens on the resume path.
    assert_script_error(
        session
            .execute("import asyncio, sys\nx = 5\nawait asyncio.sleep(0.01)\nsys.exit(4)")
            .await,
        "SystemExit: 4",
    );
    let out = session
        .execute(
            "import asyncio, sys\nawait asyncio.sleep(0.01)\nprint(x)\nsys.exit()\nprint('after')",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"5\n");
}

#[tokio::test]
async fn system_exit_while_snapshotting_is_an_error_not_an_exit() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute(
            "class Bad:\n    def __reduce__(self):\n        raise SystemExit('boom')\nbad = Bad()\nx = 1",
        )
        .await
        .unwrap();
    let err = session.snapshot_state().await.unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    let out = session.execute("print(x)").await.unwrap();
    assert_eq!(out.stdout, b"1\n");
}

// =============================================================================
// Snapshots, result variable, async state
// =============================================================================

#[tokio::test]
async fn snapshot_keeps_none_valued_variables() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session.execute("x = None").await.unwrap();
    let snapshot = session.snapshot_state().await.unwrap();

    let mut restored = InProcessSession::new(&sandbox).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    let out = restored.execute("print(x)").await.unwrap();
    assert_eq!(out.stdout, b"None\n");
}

#[tokio::test]
async fn result_variable_name_may_end_with_a_quote() {
    let sandbox = Sandbox::embedded()
        .with_result_variable("r\"")
        .build()
        .unwrap();
    let out = sandbox
        .execute("result = 1\nglobals()['r\"'] = 2")
        .await
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("2"));
}

#[tokio::test]
async fn suspended_executions_do_not_leak_their_async_state() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("import asyncio\nfor _ in range(3):\n    await asyncio.sleep(0.001)")
        .await
        .unwrap();
    let out = session
        .execute(
            "import gc, _eryx_async\n\
             gc.collect()\n\
             print(sum(isinstance(o, _eryx_async._AsyncState) for o in gc.get_objects()))",
        )
        .await
        .unwrap();
    // This execution never awaits, so no async state should be alive at all.
    assert_eq!(out.stdout, b"0\n");
}

#[tokio::test]
async fn call_soon_without_context_uses_the_current_one() {
    let out = sandbox()
        .execute(
            "import asyncio\n\
             loop = asyncio.get_running_loop()\n\
             fut = loop.create_future()\n\
             loop.call_soon(fut.set_result, 42)\n\
             print(await fut)",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"42\n");
}

#[tokio::test]
async fn stdout_behaves_like_a_text_stream() {
    let out = sandbox()
        .execute(
            "import io, sys\n\
             print(sys.stdout.isatty(), sys.stdout.name, sys.stderr.name)\n\
             sys.stdout.writelines(['a\\n', 'b\\n'])\n\
             sys.stdout.buffer.write(b'raw\\xff\\n')\n\
             try:\n    sys.stdout.fileno()\nexcept io.UnsupportedOperation:\n    print('no fileno')",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout,
        b"False <stdout> <stderr>\na\nb\nraw\xff\nno fileno\n"
    );
}

// =============================================================================
// Callbacks
// =============================================================================

struct Ping;

impl TypedCallback for Ping {
    type Args = ();

    fn name(&self) -> &str {
        "ping"
    }

    fn description(&self) -> &str {
        "Returns pong"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async { Ok(json!("pong")) })
    }
}

fn executor() -> Arc<PythonExecutor> {
    static EXECUTOR: OnceLock<Arc<PythonExecutor>> = OnceLock::new();
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

#[tokio::test]
async fn removed_callbacks_are_no_longer_defined_in_a_session() {
    let callbacks: Vec<Arc<dyn Callback>> = vec![Arc::new(Ping)];
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();
    let (callback_tx, _callback_rx) = tokio::sync::mpsc::channel(1);

    let out = session
        .execute("print('ping' in globals())")
        .with_callbacks(&callbacks, callback_tx)
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"True\n");

    let out = session
        .execute("print('ping' in globals())")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"False\n");
}

// =============================================================================
// Socket and ssl shims
// =============================================================================

/// Accepts connections and never writes, so reads (and TLS handshakes) hang
/// until the client's timeout fires.
async fn silent_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    port
}

#[tokio::test]
async fn socket_read_timeout_raises_socket_timeout() {
    let port = silent_server().await;
    let sandbox = network_sandbox(Duration::from_secs(10), Duration::from_millis(300));
    let out = sandbox
        .execute(&format!(
            "import socket\n\
             s = socket.create_connection(('127.0.0.1', {port}))\n\
             print(s.getpeername())\n\
             try:\n    s.recv(1)\n\
             except socket.timeout as e:\n    print(type(e).__name__, socket.timeout is TimeoutError, 'timed out' in str(e))"
        ))
        .await
        .unwrap();
    assert_eq!(
        out.stdout_text(),
        format!("('127.0.0.1', {port})\nTimeoutError True True\n")
    );
}

#[tokio::test]
async fn refused_connection_raises_connection_refused_error() {
    // Bind and drop a listener to get a port nothing is listening on.
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let sandbox = network_sandbox(Duration::from_secs(10), Duration::from_secs(10));
    let out = sandbox
        .execute(&format!(
            "import errno, socket\n\
             try:\n    socket.create_connection(('127.0.0.1', {port}))\n\
             except ConnectionRefusedError as e:\n    print(e.errno == errno.ECONNREFUSED)"
        ))
        .await
        .unwrap();
    assert_eq!(out.stdout, b"True\n");
}

#[tokio::test]
async fn tls_handshake_timeout_raises_timeout_error() {
    let port = silent_server().await;
    let sandbox = network_sandbox(Duration::from_millis(300), Duration::from_secs(10));
    let out = sandbox
        .execute(&format!(
            "import socket, ssl\n\
             s = socket.create_connection(('127.0.0.1', {port}))\n\
             try:\n    ssl.create_default_context().wrap_socket(s, server_hostname='localhost')\n\
             except Exception as e:\n    print(type(e).__name__, 'timed out' in str(e))"
        ))
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "TimeoutError True\n");
}
