//! WASI 0.3 guests: a minimal `wasm32-wasip3` component exporting the sandbox
//! world (tests/fixtures/p3-guest) runs, writes to stdout, and sees the
//! session's VFS, through the same linker as the p2 guests.
//!
//! The guest is built on first use, which needs the `wasm32-wasip3` target:
//! `rustup target add wasm32-wasip3`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eryx::Executor;

/// The guest component, built once per test process.
fn p3_guest() -> Arc<Executor> {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    let wasm = WASM.get_or_init(|| {
        let manifest =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/p3-guest/Cargo.toml");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("p3-guest");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "wasm32-wasip3", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .expect("running cargo");
        assert!(
            status.success(),
            "building the p3 guest failed; is the wasm32-wasip3 target installed?"
        );
        std::fs::read(target_dir.join("wasm32-wasip3/release/eryx_p3_guest.wasm")).unwrap()
    });
    Arc::new(Executor::from_binary(wasm).unwrap())
}

#[tokio::test]
async fn a_p3_guest_runs() {
    let out = p3_guest().execute("print hello").run().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
}

/// WASI 0.3 sockets get the same denial as p2's: the guest's network is
/// `eryx:net`, behind the host's policy.
#[tokio::test]
async fn a_p3_guest_has_no_wasi_sockets() {
    let out = p3_guest()
        .execute("connect 127.0.0.1:9")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "PermissionDenied\n");
}

#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn a_p3_guest_sees_the_sessions_vfs() {
    use eryx::SessionExecutor;
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};

    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    storage.mkdir("/data").await.unwrap();
    storage.write("/data/in.txt", b"from-host").await.unwrap();
    let mut session = SessionExecutor::new_with_vfs(p3_guest(), &[], storage.clone())
        .await
        .unwrap();

    let out = session
        .execute("read /data/in.txt\nwrite /data/out.txt from-guest")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-host\n");
    assert_eq!(storage.read("/data/out.txt").await.unwrap(), b"from-guest");

    // Outside the mounts there's nothing, as for p2 guests.
    let err = session.execute("read /etc/passwd").run().await.unwrap_err();
    assert!(err.to_string().contains("read /etc/passwd"), "{err}");
}
