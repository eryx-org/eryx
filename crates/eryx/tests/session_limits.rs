//! Regression coverage for limits on persistent sessions.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};

use eryx::{
    Error, InProcessSession, PythonExecutor, ResourceLimits, Sandbox, Session, SessionExecutor,
};

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

async fn limited_session(limits: ResourceLimits) -> SessionExecutor {
    SessionExecutor::new_with_limits(executor(), &[], &limits)
        .await
        .expect("session construction")
}

async fn ordinary_session() -> SessionExecutor {
    SessionExecutor::new(executor(), &[])
        .await
        .expect("session construction")
}

fn assert_memory_limit_error(error: Error) {
    let message = error.to_string().to_lowercase();
    assert!(
        message.contains("memory"),
        "unexpected memory-limit error: {error:?}"
    );
}

/// Assert that a guest write was rejected by a VFS quota.
///
/// [`eryx::vfs`] documents `QuotaExceeded` as guest-visible `ENOSPC`, but the
/// rejection currently reaches CPython's close path as a generic `OSError`.
/// This asserts only that the write failed, because the errno classification is
/// pre-existing behavior unrelated to limit propagation.
fn assert_vfs_quota_rejected(error: &Error) {
    assert!(
        matches!(error, Error::PythonException(_)) && error.to_string().contains("OSError"),
        "expected a guest-visible VFS quota rejection, got {error:?}"
    );
}

#[tokio::test]
async fn ordinary_session_still_executes_and_preserves_state() {
    let mut session = ordinary_session().await;
    // The limit-free constructor must stay limit-free: inheriting
    // `ResourceLimits::default()` here would silently add a 30 s timeout and a
    // 128 MB cap to every existing caller of `SessionExecutor::new`.
    assert_eq!(session.execution_timeout(), None);
    assert_eq!(session.fuel_limit(), None);
    session.execute("x = 41").run().await.expect("assignment");
    let output = session
        .execute("print(x + 1)")
        .run()
        .await
        .expect("stateful execution");
    assert_eq!(output.stdout.trim(), "42");
}

#[tokio::test]
async fn memory_limit_is_enforced_and_survives_reset() {
    let mut unlimited = limited_session(ResourceLimits::unlimited()).await;
    let peak = unlimited
        .execute("x = bytearray(64 * 1024 * 1024)")
        .run()
        .await
        .expect("allocation")
        .peak_memory_bytes;

    let limits = ResourceLimits::unlimited().with_max_memory_bytes(peak - 1);
    let mut session = limited_session(limits).await;
    session
        .execute("x = 1")
        .run()
        .await
        .expect("small allocation");
    assert_memory_limit_error(
        session
            .execute("x = bytearray(64 * 1024 * 1024)")
            .run()
            .await
            .expect_err("allocation should exceed the memory limit"),
    );
    session.reset(&[]).await.expect("reset");
    assert_memory_limit_error(
        session
            .execute("x = bytearray(64 * 1024 * 1024)")
            .run()
            .await
            .expect_err("memory limit must survive reset"),
    );
}

#[tokio::test]
async fn fuel_limit_is_applied_at_construction_and_reset() {
    let mut session = limited_session(ResourceLimits::unlimited().with_max_fuel(1)).await;
    assert!(
        matches!(
            session.execute("x = 1").run().await,
            Err(Error::FuelExhausted { limit: 1, .. })
        ),
        "fuel limit should be applied at construction"
    );
    session.reset(&[]).await.expect("reset");
    assert!(
        matches!(
            session.execute("x = 1").run().await,
            Err(Error::FuelExhausted { limit: 1, .. })
        ),
        "fuel limit should survive reset"
    );
}

#[tokio::test]
async fn timeout_limit_is_applied_at_construction_and_reset() {
    let limits =
        ResourceLimits::unlimited().with_execution_timeout(std::time::Duration::from_millis(100));
    let mut session = limited_session(limits).await;
    assert!(
        matches!(
            session.execute("while True: pass").run().await,
            Err(Error::Timeout(duration)) if duration == std::time::Duration::from_millis(100)
        ),
        "timeout should be applied at construction"
    );
    session.reset(&[]).await.expect("reset");
    assert!(
        matches!(
            session.execute("while True: pass").run().await,
            Err(Error::Timeout(duration)) if duration == std::time::Duration::from_millis(100)
        ),
        "timeout should survive reset"
    );
}

#[tokio::test]
async fn memory_limit_can_reject_initial_store_growth() {
    let limits = ResourceLimits::unlimited().with_max_memory_bytes(1);
    assert_memory_limit_error(
        SessionExecutor::new_with_limits(executor(), &[], &limits)
            .await
            .expect_err("an impossibly small memory limit must reject instantiation"),
    );
}

#[tokio::test]
async fn in_process_session_receives_sandbox_limits() {
    let limits = ResourceLimits::unlimited().with_max_fuel(1);
    let sandbox = Sandbox::builder()
        .with_embedded_runtime()
        .with_resource_limits(limits)
        .build()
        .expect("sandbox construction");
    let mut session = InProcessSession::new(&sandbox)
        .await
        .expect("session construction");

    assert!(
        matches!(
            session.execute("x = 1").await,
            Err(Error::FuelExhausted { limit: 1, .. })
        ),
        "InProcessSession must apply the sandbox fuel limit"
    );
    session.reset().await.expect("reset");
    assert!(
        matches!(
            session.execute("x = 1").await,
            Err(Error::FuelExhausted { limit: 1, .. })
        ),
        "InProcessSession must preserve the fuel limit after reset"
    );
}

#[tokio::test]
async fn in_process_session_applies_memory_limit_at_instantiation() {
    let sandbox = Sandbox::builder()
        .with_embedded_runtime()
        .with_resource_limits(ResourceLimits::unlimited().with_max_memory_bytes(1))
        .build()
        .expect("sandbox construction");
    let error = InProcessSession::new(&sandbox)
        .await
        .expect_err("an impossibly small memory limit must reject the session");
    assert_memory_limit_error(error);
}

#[cfg(feature = "vfs")]
#[tokio::test]
async fn caller_owned_vfs_quota_is_not_replaced_by_resource_limits() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};

    let storage = ArcStorage::new(Arc::new(InMemoryStorage::with_max_bytes(4)));
    let mut session = SessionExecutor::new_with_vfs_config_and_limits(
        executor(),
        &[],
        storage.clone(),
        eryx::VfsConfig::default(),
        &ResourceLimits::unlimited().with_max_vfs_bytes(1024),
    )
    .await
    .expect("session construction");

    session
        .execute("with open('/data/file', 'wb') as f:\n    f.write(b'1234')")
        .run()
        .await
        .expect("write within caller quota");
    assert_eq!(
        storage.read("/data/file").await.expect("read baseline"),
        b"1234"
    );

    let result = session
        .execute("with open('/data/file', 'ab') as f:\n    f.write(b'5')")
        .run()
        .await;
    let error = result.expect_err("caller storage quota must reject the write");
    assert_vfs_quota_rejected(&error);
    assert_eq!(
        storage
            .read("/data/file")
            .await
            .expect("read after failed write"),
        b"1234"
    );
    session.reset(&[]).await.expect("reset");
    assert_eq!(
        storage.read("/data/file").await.expect("read after reset"),
        b"1234"
    );
    let error = session
        .execute("with open('/data/file', 'ab') as f:\n    f.write(b'5')")
        .run()
        .await
        .expect_err("caller quota must survive reset");
    assert_vfs_quota_rejected(&error);
}

#[cfg(feature = "vfs")]
#[tokio::test]
async fn owned_vfs_quota_survives_reset() {
    let limits = ResourceLimits::unlimited().with_max_vfs_bytes(4);
    let mut session = SessionExecutor::new_with_limits(executor(), &[], &limits)
        .await
        .expect("session construction");
    session
        .execute("with open('/data/file', 'wb') as f:\n    f.write(b'1234')")
        .run()
        .await
        .expect("write within owned quota");
    let error = session
        .execute("with open('/data/file', 'ab') as f:\n    f.write(b'5')")
        .run()
        .await
        .expect_err("owned quota must reject growth");
    assert_vfs_quota_rejected(&error);
    session
        .execute("with open('/data/file', 'rb') as f:\n    assert f.read() == b'1234'")
        .run()
        .await
        .expect("owned data should survive rejected append");
    session.reset(&[]).await.expect("reset");
    let error = session
        .execute("with open('/data/file', 'ab') as f:\n    f.write(b'5')")
        .run()
        .await
        .expect_err("owned quota must survive reset");
    assert_vfs_quota_rejected(&error);
    session
        .execute("with open('/data/file', 'rb') as f:\n    assert f.read() == b'1234'")
        .run()
        .await
        .expect("owned data should survive reset");
}
