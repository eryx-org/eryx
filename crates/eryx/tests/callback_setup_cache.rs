//! Coverage for the guest's idempotent callback installation.
//!
//! The guest only reinstalls its callbacks when the host's callback
//! declarations differ from the ones already installed, and installing
//! compiles no Python. These tests check that the wrappers keep working
//! across executions that reuse the same set, that changes to the set are
//! picked up, and that the installation is refreshed after the operations
//! that can disturb it.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use eryx::callback_handler::run_callback_handler;
use eryx::{
    Callback, CallbackError, JsonSchema, PythonExecutor, ResourceLimits, Sandbox, SessionExecutor,
    TypedCallback,
};
use serde::Deserialize;
use serde_json::{Value, json};

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

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// Data to echo back
    data: Value,
}

struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input data back"
    }

    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(args.data) })
    }
}

struct PingCallback;

impl TypedCallback for PingCallback {
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
        Box::pin(async move { Ok(json!("pong")) })
    }
}

fn echo_only() -> Vec<Arc<dyn Callback>> {
    vec![Arc::new(EchoCallback)]
}

fn echo_and_ping() -> Vec<Arc<dyn Callback>> {
    vec![Arc::new(EchoCallback), Arc::new(PingCallback)]
}

/// Run `code` on `session` with `callbacks` available, serving their
/// invocations with the same handler `Sandbox` uses.
async fn run_result(
    session: &mut SessionExecutor,
    callbacks: &[Arc<dyn Callback>],
    code: &str,
) -> Result<Vec<u8>, eryx::Error> {
    let (callback_tx, callback_rx) = tokio::sync::mpsc::channel(4);
    let callbacks_map: HashMap<String, Arc<dyn Callback>> = callbacks
        .iter()
        .map(|cb| (cb.name().to_string(), Arc::clone(cb)))
        .collect();
    let handler = tokio::spawn(run_callback_handler(
        callback_rx,
        Arc::new(callbacks_map),
        ResourceLimits::unlimited(),
        Arc::new(HashMap::new()),
    ));

    let result = session
        .execute(code)
        .with_callbacks(callbacks, callback_tx)
        .run()
        .await
        .map(|output| output.stdout);
    handler.await.unwrap();
    result
}

async fn run(
    session: &mut SessionExecutor,
    callbacks: &[Arc<dyn Callback>],
    code: &str,
) -> Vec<u8> {
    run_result(session, callbacks, code)
        .await
        .expect("execution failed")
}

#[tokio::test]
async fn wrappers_keep_working_when_the_callback_set_is_unchanged() {
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();

    for i in 0..3 {
        let code = format!("print(await echo(data={i}))");
        assert_eq!(
            run(&mut session, &callbacks, &code).await,
            format!("{i}\n").into_bytes()
        );
    }
}

#[tokio::test]
async fn a_changed_callback_set_is_installed() {
    let mut session = SessionExecutor::new(executor(), &echo_only())
        .await
        .unwrap();

    assert_eq!(
        run(
            &mut session,
            &echo_only(),
            "print(sorted(c['name'] for c in list_callbacks()))"
        )
        .await,
        b"['echo']\n"
    );

    // Adding a callback exposes its wrapper and updates introspection.
    assert_eq!(
        run(
            &mut session,
            &echo_and_ping(),
            "print(sorted(c['name'] for c in list_callbacks()), await ping())"
        )
        .await,
        b"['echo', 'ping'] pong\n"
    );

    // Going back to the smaller set is picked up too.
    assert_eq!(
        run(
            &mut session,
            &echo_only(),
            "print(sorted(c['name'] for c in list_callbacks()))"
        )
        .await,
        b"['echo']\n"
    );
}

#[tokio::test]
async fn callbacks_survive_clear_state() {
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();

    assert_eq!(
        run(
            &mut session,
            &callbacks,
            "x = 1\nprint(await echo(data='a'))\necho = 'tampered'"
        )
        .await,
        b"a\n"
    );
    session.clear_state().await.unwrap();
    assert_eq!(
        run(
            &mut session,
            &callbacks,
            "print('x' in globals(), await echo(data='b'))"
        )
        .await,
        b"False b\n"
    );
}

/// Installs an audit hook that records the source of every compile other
/// than the user's own code, i.e. compiles done by the runtime on the user's
/// behalf, in `sys._eryx_runtime_compiles`.
///
/// Executions measured with it don't await callbacks: resuming a suspended
/// execution runs small snippets that have nothing to do with setup.
const RECORD_RUNTIME_COMPILES: &str = r#"
import sys
sys._eryx_runtime_compiles = []
def _eryx_record_runtime_compiles(event, args, _sys=sys):
    if event == 'compile' and args[1] not in ('<user>', '<preamble>'):
        _sys._eryx_runtime_compiles.append(args[0])
sys.addaudithook(_eryx_record_runtime_compiles)
"#;

#[tokio::test]
async fn callback_setup_compiles_nothing_after_the_first_install() {
    let initial_callbacks = echo_only();
    let changed_callbacks = echo_and_ping();
    let mut session = SessionExecutor::new(executor(), &initial_callbacks)
        .await
        .unwrap();
    run(&mut session, &initial_callbacks, RECORD_RUNTIME_COMPILES).await;

    // Same set.
    assert_eq!(
        run(&mut session, &initial_callbacks, "print(callable(echo))").await,
        b"True\n"
    );
    // Same set after clear.
    session.clear_state().await.unwrap();
    assert_eq!(
        run(&mut session, &initial_callbacks, "print(callable(echo))").await,
        b"True\n"
    );
    // Changed set.
    assert_eq!(
        run(&mut session, &changed_callbacks, "print(callable(ping))").await,
        b"True\n"
    );
    // Changed set after clear, then check the hook does see compiles.
    session.clear_state().await.unwrap();
    assert_eq!(
        run(
            &mut session,
            &changed_callbacks,
            "import sys\nbefore = len(sys._eryx_runtime_compiles)\ncompile('1', '<probe>', 'exec')\nprint(before, len(sys._eryx_runtime_compiles), callable(ping))"
        )
        .await,
        b"0 1 True\n"
    );
}

#[tokio::test]
async fn callback_setup_compiles_nothing_after_restore() {
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();
    run(&mut session, &callbacks, "x = 41").await;
    let snapshot = session.snapshot_state().await.unwrap();

    let mut restored = SessionExecutor::new(executor(), &callbacks).await.unwrap();
    // Import dill up front so restoring doesn't compile it.
    let setup = format!("import dill\n{RECORD_RUNTIME_COMPILES}");
    run(&mut restored, &callbacks, &setup).await;
    restored.restore_state(&snapshot).await.unwrap();
    // restore_state still compiles its own script; nothing else may compile.
    assert_eq!(
        run(
            &mut restored,
            &callbacks,
            "import sys\nprint([s for s in sys._eryx_runtime_compiles if b'_eryx_restore_bytes' not in s], callable(echo), x + 1)"
        )
        .await,
        b"[] True 42\n"
    );
}

#[tokio::test]
async fn callback_setup_does_not_use_the_exec_builtin() {
    let callbacks: Vec<Arc<dyn Callback>> = Vec::new();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();

    run(
        &mut session,
        &callbacks,
        r#"
import sys
import builtins
sys._eryx_original_exec = builtins.exec
builtins.exec = lambda *args: None
invoke = None
list_callbacks = None
"#,
    )
    .await;

    session.clear_state().await.unwrap();
    // Top-level await runs the code as a coroutine, without builtins.exec.
    assert_eq!(
        run(
            &mut session,
            &callbacks,
            r#"
import asyncio, builtins, sys
builtins.exec = sys._eryx_original_exec
await asyncio.sleep(0)
print(callable(invoke), list_callbacks())
"#
        )
        .await,
        b"True []\n"
    );
}

#[tokio::test]
async fn failed_callback_setup_is_retried() {
    let empty_callbacks: Vec<Arc<dyn Callback>> = Vec::new();
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &empty_callbacks)
        .await
        .unwrap();

    // Setup parses the callback declarations with json.loads; fail it once.
    run(
        &mut session,
        &empty_callbacks,
        r#"
import json
_original_loads = json.loads
def _fail_once(*args, **kwargs):
    json.loads = _original_loads
    raise RuntimeError('fail setup once')
json.loads = _fail_once
"#,
    )
    .await;

    let error = run_result(&mut session, &callbacks, "print(await echo(data='retry'))")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fail setup once"), "{error}");
    assert_eq!(
        run(&mut session, &callbacks, "print(await echo(data='retry'))").await,
        b"retry\n"
    );
}

#[tokio::test]
async fn callbacks_survive_snapshot_and_restore() {
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();

    assert_eq!(
        run(
            &mut session,
            &callbacks,
            "x = 41\nprint(await echo(data=x))"
        )
        .await,
        b"41\n"
    );
    let snapshot = session.snapshot_state().await.unwrap();

    let mut restored = SessionExecutor::new(executor(), &callbacks).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    assert_eq!(
        run(&mut restored, &callbacks, "print(await echo(data=x + 1))").await,
        b"42\n"
    );
}

#[tokio::test]
async fn stateless_sandboxes_with_callbacks_work_repeatedly() {
    let sandbox = Sandbox::embedded()
        .with_callback(EchoCallback)
        .build()
        .unwrap();

    for i in 0..3 {
        let output = sandbox
            .execute(&format!("print(await echo(data={i}))"))
            .await
            .unwrap();
        assert_eq!(output.stdout, format!("{i}\n").into_bytes());
    }
}
