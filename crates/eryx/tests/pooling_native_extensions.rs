//! A runtime with numpy's native extensions linked in must instantiate under
//! the pooling allocator.
//!
//! Each extension adds its own core instance and its lifted and lowered
//! functions to the component, which outgrew wasmtime's default 1 MiB
//! instance metadata limit and failed every instantiation with "component
//! instance size does not fit in pooling allocator requirements".
//!
//! Needs `ERYX_TEST_NUMPY_WASI` set to the path of `numpy-wasi.tar.gz` (the
//! `demo-assets` release, also fetched by `mise run download-numpy-wasi`).
//! The allocator is read when the process-global engine is created, so this
//! binary holds a single test that sets it before anything touches the engine.

#![cfg(feature = "native-extensions")]
#![allow(unsafe_code, clippy::expect_used, clippy::unwrap_used)]

use eryx::Sandbox;

#[tokio::test]
async fn numpy_instantiates_under_the_pooling_allocator() {
    let Ok(numpy) = std::env::var("ERYX_TEST_NUMPY_WASI") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "ERYX_TEST_NUMPY_WASI must be set in CI"
        );
        eprintln!("skipping: ERYX_TEST_NUMPY_WASI is not set");
        return;
    };

    // SAFETY: the current-thread runtime has no other threads yet; this is the
    // only test in the binary.
    unsafe {
        std::env::set_var("ERYX_ALLOCATOR", "pooling");
    }

    let sandbox = Sandbox::embedded()
        .with_package(numpy)
        .unwrap()
        .build()
        .unwrap();
    let output = sandbox
        .execute("import numpy as np\nprint(np.arange(5).sum())")
        .await
        .unwrap();
    assert_eq!(output.stdout_text().trim(), "10");
}
