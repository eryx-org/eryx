//! Regression tests for sandbox construction, execution handles and the pool.

#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;

use eryx::{PackageFormat, Sandbox};

/// A pure-Python wheel containing a single `corepkg` module.
fn tiny_wheel() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buf);
    writer
        .start_file(
            "corepkg/__init__.py",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"VALUE = 42\n").unwrap();
    writer.finish().unwrap();
    buf.into_inner()
}

#[tokio::test]
async fn executor_keeps_packages_after_sandbox_is_dropped() {
    let sandbox = Sandbox::embedded()
        .with_package_bytes(&tiny_wheel(), PackageFormat::Wheel, "corepkg")
        .unwrap()
        .build()
        .unwrap();
    let executor = sandbox.executor();
    drop(sandbox);

    // The preinitialized runtime fixes `sys.path` at snapshot time, so add the
    // mount explicitly; what matters here is that the directory still exists.
    let output = executor
        .execute("import sys\nsys.path.insert(0, '/site-packages')\nimport corepkg\nprint(corepkg.VALUE)")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[tokio::test]
async fn execution_handle_reports_completion() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let handle = sandbox.execute_cancellable("x = 1 + 1");
    let token = handle.cancellation_token();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.is_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "still running after completing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!token.is_cancelled());
    handle.wait().await.unwrap();
    // Waiting to completion must not cancel the shared token.
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn dropping_execution_handle_cancels_execution() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let handle = sandbox.execute_cancellable("while True: pass");
    let token = handle.cancellation_token();
    drop(handle);
    assert!(token.is_cancelled());
}
