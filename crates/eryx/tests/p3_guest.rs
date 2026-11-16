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
    Arc::new(Executor::from_binary(p3_wasm()).unwrap())
}

/// The guest component's bytes, built once per test process.
fn p3_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
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
    })
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

/// A p3 guest gets the same limits from the hybrid VFS as a p2 guest: it
/// can't climb out of a host mount with `..`, can't write to a read-only
/// mount, and can't write past the storage's quota.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn a_p3_guest_is_held_to_the_mounts_limits() {
    use eryx::SessionExecutor;
    use eryx::session::{VfsConfig, VolumeMount};
    use eryx::vfs::{ArcStorage, InMemoryStorage};

    // A host directory mounted read-only, next to a file that isn't mounted.
    let host = tempfile::tempdir().unwrap();
    std::fs::create_dir(host.path().join("mounted")).unwrap();
    std::fs::write(host.path().join("mounted/inside.txt"), "inside").unwrap();
    std::fs::write(host.path().join("outside.txt"), "outside").unwrap();
    let config = VfsConfig::default().with_volumes(vec![VolumeMount::read_only(
        host.path().join("mounted"),
        "/mnt/ro",
    )]);
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::with_max_bytes(1024)));
    let mut session = SessionExecutor::new_with_vfs_config(p3_guest(), &[], storage, config)
        .await
        .unwrap();

    let out = session
        .execute("read /mnt/ro/inside.txt")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "inside\n");

    // Each fails in the guest's own filesystem call, which it reports as
    // `<command> <path>: <error>`.
    let fails = async |session: &mut SessionExecutor, code: &str, failed: &str| {
        let err = session.execute(code).run().await.unwrap_err();
        assert!(err.to_string().contains(failed), "{code}: {err}");
    };
    for escape in [
        "read /mnt/ro/../outside.txt",
        "read /mnt/ro/../../../../etc/passwd",
    ] {
        fails(&mut session, escape, &format!("{escape}: No such file")).await;
    }
    let read_only = "write /mnt/ro/new.txt nope";
    fails(
        &mut session,
        read_only,
        "write /mnt/ro/new.txt: Permission denied",
    )
    .await;
    let over_quota = format!("write /data/big.txt {}", "x".repeat(4096));
    fails(
        &mut session,
        &over_quota,
        "write /data/big.txt: No space left",
    )
    .await;
    assert!(!host.path().join("mounted/new.txt").exists());
}

/// A p3 guest running as the shell gets the shell's filesystem (#832), as
/// p2 shells do: a private `/tmp`, `/dev` and the read-only root, and no host
/// paths. Also built without `unstable-vfs`, where the `shell` feature is
/// what links the hybrid VFS.
#[cfg(feature = "shell")]
#[tokio::test]
async fn a_p3_shell_guest_gets_the_shells_filesystem() {
    let shell = Executor::from_binary(p3_wasm())
        .unwrap()
        .with_language(eryx::Language::Shell);
    let out = shell
        .execute("write /tmp/x from-guest\nread /tmp/x\nread /dev/null")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-guest\n\n");

    // A host file next to the sandbox isn't reachable, and nothing can be
    // written above the mounts.
    let host = tempfile::tempdir().unwrap();
    let host_file = host.path().join("host.txt");
    std::fs::write(&host_file, "host").unwrap();
    let host_read = format!("read {}", host_file.display());
    for (code, failed) in [
        (host_read.as_str(), "No such file"),
        ("read /etc/passwd", "read /etc/passwd: No such file"),
        (
            "write /new.txt nope",
            "write /new.txt: Operation not permitted",
        ),
    ] {
        let err = shell.execute(code).run().await.unwrap_err();
        assert!(err.to_string().contains(failed), "{code}: {err}");
    }

    // A guest that isn't the shell keeps its filesystem: no /tmp of its own.
    #[cfg(not(feature = "unstable-vfs"))]
    {
        let err = p3_guest()
            .execute("write /tmp/x nope")
            .run()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("write /tmp/x"), "{err}");
    }
}
