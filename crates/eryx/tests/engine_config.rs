//! An [`Engine`] built from an [`EngineConfig`] uses its own allocator
//! limits and ignores the `ERYX_*` environment variables, which only
//! configure the shared engine.
//!
//! The test sets environment variables, so this binary holds a single test.
//! It never touches the shared engine, which the invalid environment below
//! would stop from starting.

#![cfg(feature = "embedded")]
#![allow(unsafe_code, clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use eryx::{Engine, EngineConfig, InProcessSession, InstanceAllocator, ResourceLimits, Sandbox};

#[tokio::test]
async fn a_code_built_engine_ignores_the_environment() {
    // SAFETY: the current-thread runtime has no other threads yet; this is the
    // only test in the binary.
    unsafe {
        std::env::set_var("ERYX_ALLOCATOR", "on-demand");
        std::env::set_var("ERYX_POOL_INSTANCES", "1");
        // Invalid: the shared engine would refuse to start with it.
        std::env::set_var("ERYX_POOL_KEEP_RESIDENT_MB", "lots");
    }

    Engine::new(EngineConfig::default().with_pool_instances(0))
        .expect_err("zero pool instances must be rejected");
    let engine = Engine::new(
        EngineConfig::default()
            .with_allocator(InstanceAllocator::Pooling)
            .with_pool_instances(2)
            .with_warm_instances(0),
    )
    .expect("the invalid environment variable is not read");

    let sandbox = Sandbox::embedded()
        .with_engine(engine.clone())
        .build()
        .unwrap();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        sessions.push(
            InProcessSession::new(&sandbox)
                .await
                .expect("the engine's pool of 2 applies, not the environment's 1"),
        );
    }
    let Err(err) = InProcessSession::new(&sandbox).await else {
        panic!("a third sandbox must not fit in a pool of two");
    };
    assert!(
        err.to_string()
            .contains("concurrent limit of 2 for component instances"),
        "{err}"
    );
    drop(sessions);

    // The epoch ticker advances this engine too, so timeouts still fire.
    let timed = Sandbox::embedded()
        .with_engine(engine)
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let err = timed
        .execute("while True: pass")
        .await
        .expect_err("the loop must time out");
    assert!(matches!(err, eryx::Error::Timeout(_)), "{err:?}");
}
