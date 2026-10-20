//! The pooling allocator's resident memory for unused slots is released once
//! the pool has been idle for `ERYX_POOL_IDLE_RELEASE_SECS`.
//!
//! The setting is read when the process-global engine is created, so this
//! binary holds a single test that sets it before anything touches the engine.

#![cfg(feature = "embedded")]
#![allow(unsafe_code, clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use eryx::Sandbox;

#[test]
fn idle_pool_memory_is_released() {
    // SAFETY: no other threads exist yet; this is the only test in the binary.
    unsafe {
        std::env::set_var("ERYX_POOL_IDLE_RELEASE_SECS", "1");
        std::env::set_var("ERYX_WARM_INSTANCES", "0");
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sandbox = Sandbox::embedded().build().unwrap();
    rt.block_on(sandbox.execute("data = bytearray(4 << 20)"))
        .unwrap();

    let executor = sandbox.executor();
    let resident = || {
        executor
            .engine()
            .pooling_allocator_metrics()
            .expect("the shared engine uses the pooling allocator")
            .unused_memory_bytes_resident()
    };
    assert!(resident() > 0, "a used slot keeps its memory resident");

    let deadline = Instant::now() + Duration::from_secs(10);
    while resident() > 0 {
        assert!(
            Instant::now() < deadline,
            "idle memory was not released: {} bytes resident",
            resident()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
