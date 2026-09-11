//! Coverage for the guest's idempotent callback installation.
//!
//! The guest only runs its callback setup script when the host's callback
//! declarations differ from the ones already installed. These tests check
//! that the wrappers keep working across executions that reuse the same set,
//! that changes to the set are picked up, and that the installation is
//! refreshed after the operations that can disturb it.
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
async fn run(session: &mut SessionExecutor, callbacks: &[Arc<dyn Callback>], code: &str) -> String {
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

    let output = session
        .execute(code)
        .with_callbacks(callbacks, callback_tx)
        .run()
        .await
        .expect("execution failed");
    handler.await.unwrap();
    output.stdout
}

#[tokio::test]
async fn wrappers_keep_working_when_the_callback_set_is_unchanged() {
    let callbacks = echo_only();
    let mut session = SessionExecutor::new(executor(), &callbacks).await.unwrap();

    for i in 0..3 {
        let code = format!("print(await echo(data={i}))");
        assert_eq!(run(&mut session, &callbacks, &code).await, i.to_string());
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
        "['echo']"
    );

    // Adding a callback exposes its wrapper and updates introspection.
    assert_eq!(
        run(
            &mut session,
            &echo_and_ping(),
            "print(sorted(c['name'] for c in list_callbacks()), await ping())"
        )
        .await,
        "['echo', 'ping'] pong"
    );

    // Going back to the smaller set is picked up too.
    assert_eq!(
        run(
            &mut session,
            &echo_only(),
            "print(sorted(c['name'] for c in list_callbacks()))"
        )
        .await,
        "['echo']"
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
            "x = 1\nprint(await echo(data='a'))"
        )
        .await,
        "a"
    );
    session.clear_state().await.unwrap();
    assert_eq!(
        run(
            &mut session,
            &callbacks,
            "print('x' in globals(), await echo(data='b'))"
        )
        .await,
        "False b"
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
        "41"
    );
    let snapshot = session.snapshot_state().await.unwrap();

    let mut restored = SessionExecutor::new(executor(), &callbacks).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    assert_eq!(
        run(&mut restored, &callbacks, "print(await echo(data=x + 1))").await,
        "42"
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
        assert_eq!(output.stdout, i.to_string());
    }
}
