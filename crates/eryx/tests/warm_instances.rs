//! Coverage for the warm-instance pool behind stateless execution.
//!
//! After a `Sandbox::execute()` on a multi-threaded runtime, a background task
//! pre-instantiates a replacement store so the next execution skips
//! instantiation. These tests check that the pool fills, that executions on a
//! pre-instantiated store see exactly the per-execution state they would on a
//! fresh one, and that the pool stays out of the way where it cannot help.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use eryx::{CallbackError, JsonSchema, ResourceLimits, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::Value;

/// Wait for the background task to top the pool up, returning the ready count.
async fn wait_for_warm(sandbox: &Sandbox) -> usize {
    let executor = sandbox.executor();
    for _ in 0..500 {
        let ready = executor.warm_instances_ready();
        if ready > 0 {
            return ready;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    executor.warm_instances_ready()
}

/// Build a sandbox and run one execution so that a warm instance is waiting.
async fn sandbox_with_warm_instance() -> Sandbox {
    let sandbox = Sandbox::embedded().build().unwrap();
    sandbox.execute("pass").await.unwrap();
    assert_eq!(wait_for_warm(&sandbox).await, 1);
    sandbox
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_is_prepared_after_each_execution() {
    let sandbox = Sandbox::embedded().build().unwrap();
    assert_eq!(sandbox.executor().warm_instances_ready(), 0);

    let cold = sandbox.execute("print('cold')").await.unwrap();
    assert_eq!(cold.stdout, "cold");
    assert_eq!(wait_for_warm(&sandbox).await, 1);

    let warm = sandbox
        .execute("import json\nprint(json.dumps({'warm': True}))")
        .await
        .unwrap();
    assert_eq!(warm.stdout, "{\"warm\": true}");
    assert_eq!(wait_for_warm(&sandbox).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_instances_are_shared_between_sandboxes_of_the_same_runtime() {
    let _first = sandbox_with_warm_instance().await;

    // A brand-new sandbox (the SandboxFactory pattern) finds the instance
    // the first one left behind.
    let second = Sandbox::embedded().build().unwrap();
    assert_eq!(second.executor().warm_instances_ready(), 1);
    let output = second.execute("print('shared')").await.unwrap();
    assert_eq!(output.stdout, "shared");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executions_on_warm_instances_are_isolated() {
    let sandbox = sandbox_with_warm_instance().await;

    sandbox
        .execute("leaked = 'state'\nprint('one')")
        .await
        .unwrap();
    wait_for_warm(&sandbox).await;

    let output = sandbox
        .execute("print('leaked' in globals())")
        .await
        .unwrap();
    assert_eq!(output.stdout, "False");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_execution_configuration_applies_to_a_warm_instance() {
    // The instance waiting in the pool was instantiated by a sandbox with no
    // callbacks and the default result variable.
    let _plain = sandbox_with_warm_instance().await;

    let configured = Sandbox::embedded()
        .with_callback(EchoCallback)
        .with_result_variable("outcome")
        .build()
        .unwrap();
    assert_eq!(configured.executor().warm_instances_ready(), 1);

    let output = configured
        .execute("outcome = await echo(data={'n': 1})\nprint(outcome)")
        .await
        .unwrap();
    assert_eq!(output.stdout, "{'n': 1}");
    assert_eq!(output.result.as_deref(), Some(r#"{"n": 1}"#));
}

#[cfg(feature = "vfs")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_virtual_filesystem_is_mounted_on_a_warm_instance() {
    let sandbox = sandbox_with_warm_instance().await;

    let output = sandbox
        .execute(
            "with open('/data/note.txt', 'w') as f:\n    f.write('hi')\nprint(open('/data/note.txt').read())",
        )
        .await
        .unwrap();
    assert_eq!(output.stdout, "hi");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_limit_below_the_snapshot_baseline_is_still_rejected() {
    let limits = ResourceLimits::unlimited().with_max_memory_bytes(1024 * 1024);
    let limited = Sandbox::embedded()
        .with_resource_limits(limits)
        .build()
        .unwrap();

    let cold_error = limited
        .execute("pass")
        .await
        .expect_err("1 MiB cannot hold the snapshot");
    assert!(
        cold_error.to_string().to_lowercase().contains("memory"),
        "unexpected error: {cold_error:?}"
    );

    // With an instance ready, the limited execution must not silently run on
    // it: the pool leaves it alone and the same error comes back.
    let _plain = sandbox_with_warm_instance().await;
    let warm_error = limited
        .execute("pass")
        .await
        .expect_err("1 MiB cannot hold the snapshot");
    assert!(
        warm_error.to_string().to_lowercase().contains("memory"),
        "unexpected error: {warm_error:?}"
    );
    assert_eq!(limited.executor().warm_instances_ready(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuel_limits_apply_to_a_warm_instance() {
    let _plain = sandbox_with_warm_instance().await;

    let limits = ResourceLimits::unlimited().with_max_fuel(10_000);
    let limited = Sandbox::embedded()
        .with_resource_limits(limits)
        .build()
        .unwrap();
    assert_eq!(limited.executor().warm_instances_ready(), 1);

    let error = limited
        .execute("total = sum(range(1_000_000))")
        .await
        .expect_err("the loop needs far more than 10k fuel");
    assert!(
        matches!(error, eryx::Error::FuelExhausted { .. }),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn the_pool_is_inactive_on_a_current_thread_runtime() {
    let sandbox = Sandbox::embedded().build().unwrap();
    sandbox.execute("pass").await.unwrap();
    sandbox.execute("pass").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sandbox.executor().warm_instances_ready(), 0);
}
