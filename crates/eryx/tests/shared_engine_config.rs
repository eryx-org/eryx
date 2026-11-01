//! `configure_shared_engine` sets the process-global engine configuration, taking
//! precedence over the environment, and only until the engine exists.
//!
//! The configuration is fixed once per process, so this binary holds a single
//! test.

#![cfg(feature = "embedded")]
#![allow(unsafe_code, clippy::expect_used, clippy::unwrap_used)]

use eryx::{
    InProcessSession, InstanceAllocator, Sandbox, SharedEngineConfig, configure_shared_engine,
};

#[tokio::test]
async fn configure_shared_engine_beats_the_environment_until_the_engine_exists() {
    // SAFETY: the current-thread runtime has no other threads yet; this is the
    // only test in the binary.
    unsafe {
        std::env::set_var("ERYX_POOL_INSTANCES", "1");
    }

    // A rejected configuration leaves the slot open for a valid one.
    configure_shared_engine(SharedEngineConfig::default().with_pool_instances(0))
        .expect_err("zero pool instances must be rejected");
    configure_shared_engine(
        SharedEngineConfig::default()
            .with_allocator(InstanceAllocator::Pooling)
            .with_pool_instances(2)
            .with_warm_instances(0),
    )
    .unwrap();

    let sandbox = Sandbox::embedded().build().unwrap();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        sessions.push(
            InProcessSession::new(&sandbox)
                .await
                .expect("the explicit pool size of 2 applies, not the environment's 1"),
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

    let err = configure_shared_engine(SharedEngineConfig::default().with_pool_instances(10))
        .expect_err("the engine already exists");
    assert!(err.to_string().contains("already fixed"), "{err}");
    drop(sessions);
}
