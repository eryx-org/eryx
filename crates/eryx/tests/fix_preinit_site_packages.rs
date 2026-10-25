//! Packages added with `with_package_bytes` must be importable on the default
//! (pre-initialized) embedded runtime, whose snapshot fixes `sys.path`.

#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;

use eryx::{InProcessSession, PackageFormat, Sandbox, Session};

/// A pure-Python wheel containing a single `name` module with `VALUE = value`.
fn wheel(name: &str, value: u32) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buf);
    writer
        .start_file(
            format!("{name}/__init__.py"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writeln!(writer, "VALUE = {value}").unwrap();
    writer.finish().unwrap();
    buf.into_inner()
}

fn sandbox_with_two_packages() -> Sandbox {
    Sandbox::embedded()
        .with_package_bytes(&wheel("firstpkg", 1), PackageFormat::Wheel, "firstpkg")
        .unwrap()
        .with_package_bytes(&wheel("secondpkg", 2), PackageFormat::Wheel, "secondpkg")
        .unwrap()
        .build()
        .unwrap()
}

#[tokio::test]
async fn packages_are_importable() {
    let sandbox = sandbox_with_two_packages();
    let out = sandbox
        .execute("import firstpkg, secondpkg\nprint(firstpkg.VALUE, secondpkg.VALUE)")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"1 2\n");
    // A second execution on a reused instance still sees them, once each.
    let out = sandbox
        .execute("import sys\nprint(sys.path.count('/site-packages'), sys.path.count('/site-packages-1'))")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"1 1\n");
}

#[tokio::test]
async fn packages_are_importable_in_a_session() {
    let sandbox = sandbox_with_two_packages();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session.execute("x = 1").await.unwrap();
    let out = session
        .execute("import secondpkg\nprint(secondpkg.VALUE + x)")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"3\n");
}

#[tokio::test]
async fn no_site_packages_without_packages() {
    let out = Sandbox::embedded()
        .build()
        .unwrap()
        .execute("import sys\nprint([p for p in sys.path if 'site-packages' in p])")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"[]\n");
}

#[tokio::test]
async fn packages_do_not_shadow_the_standard_library() {
    let out = sandbox_with_two_packages()
        .execute("import sys\nprint(sys.path.index('/python-stdlib') < sys.path.index('/site-packages'))")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"True\n");
}

#[tokio::test]
async fn restoring_a_snapshot_that_uses_a_package_before_any_execution() {
    let sandbox = sandbox_with_two_packages();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("import firstpkg\nmodule = firstpkg")
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();

    // A fresh session restores before it has executed anything.
    let mut restored = InProcessSession::new(&sandbox).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    let out = restored.execute("print(module.VALUE)").await.unwrap();
    assert_eq!(out.stdout, b"1\n");
}
