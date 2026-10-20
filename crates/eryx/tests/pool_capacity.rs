//! `ERYX_POOL_INSTANCES` is the number of sandboxes the pooling allocator can
//! hold at once, whatever each one needs from the pool internally (13 core
//! instances, 2 tables and a fiber stack per in-flight guest task).
//!
//! The setting is read when the process-global engine is created, so this
//! binary holds a single test that sets it before anything touches the engine.

#![cfg(feature = "embedded")]
#![allow(unsafe_code, clippy::expect_used, clippy::unwrap_used)]

use eryx::InProcessSession;
use eryx::Sandbox;
use eryx::session::Session;

#[tokio::test]
async fn the_pool_holds_exactly_the_configured_number_of_sandboxes() {
    // SAFETY: the current-thread runtime has no other threads yet; this is the
    // only test in the binary.
    unsafe {
        std::env::set_var("ERYX_ALLOCATOR", "pooling");
        std::env::set_var("ERYX_POOL_INSTANCES", "3");
        std::env::set_var("ERYX_WARM_INSTANCES", "0");
    }

    let sandbox = Sandbox::embedded().build().unwrap();
    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(InProcessSession::new(&sandbox).await.unwrap());
    }
    // Concurrent asyncio work holds several guest-task fibers per sandbox,
    // which must also fit while every sandbox is alive.
    for session in &mut sessions {
        session
            .execute(
                "import asyncio\n\
                 asyncio.ensure_future(asyncio.sleep(30))\n\
                 await asyncio.wait_for(asyncio.gather(*(asyncio.sleep(0.01) for _ in range(4))), 5)",
            )
            .await
            .unwrap();
    }

    let Err(err) = InProcessSession::new(&sandbox).await else {
        panic!("a fourth sandbox must not fit in a pool of three");
    };
    assert!(
        err.to_string()
            .contains("concurrent limit of 3 for component instances"),
        "the pool should run out of sandboxes, not of something inside one: {err}"
    );
}
