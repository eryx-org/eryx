//! External commands for the shell guest (`eryx:process`): registered
//! [`Command`]s run as child processes of the embedded shell runtime.
//!
//! Most tests use a tiny command built by the test itself
//! (tests/fixtures/test-cmd, which needs `rustup target add wasm32-wasip2`).
//!
//! ```sh
//! mise run precompile-runtime-shell
//! cargo nextest run -p eryx --features embedded,shell,unstable-vfs --test commands
//! ```
#![cfg(all(feature = "embedded", feature = "shell"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eryx::language::Shell;
use eryx::{Command, ErrorKind, Sandbox, SandboxBuilder, SessionExecutor, state};

/// The test command, built once per test process.
fn test_cmd() -> Command {
    Command::from_bytes(&test_cmd_wasm()).unwrap()
}

fn test_cmd_wasm() -> Vec<u8> {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let manifest =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-cmd/Cargo.toml");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("test-cmd");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "wasm32-wasip2", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .expect("running cargo");
        assert!(
            status.success(),
            "building the test command failed; is the wasm32-wasip2 target installed?"
        );
        std::fs::read(target_dir.join("wasm32-wasip2/release/eryx-test-cmd.wasm")).unwrap()
    })
    .clone()
}

fn shell() -> SandboxBuilder<Shell, state::Has> {
    Sandbox::embedded().shell()
}

#[tokio::test]
async fn pipelines_spawn_child_components() {
    let sandbox = shell().with_command("upper", test_cmd()).build().unwrap();

    let out = sandbox
        .execute(
            "printf 'hi\\n' | upper; echo lo | upper | upper; x=$(echo sub | upper); echo \"x=$x\"",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "HI\nLO\nx=SUB\n");
}

/// One command under several names runs with each name as `argv[0]`, which
/// is how a multicall binary (coreutils) picks the tool.
#[tokio::test]
async fn a_multicall_command_sees_its_name_as_argv0() {
    let sandbox = shell()
        .with_commands(["one", "two"], test_cmd())
        .build()
        .unwrap();

    let out = sandbox.execute("one argv0; two argv0").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn exit_status_stderr_and_env_reach_the_shell() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();

    let out = sandbox
        .execute("tc exit 3; echo \"status=$?\"; FOO=bar tc env FOO; tc env PWD")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "status=3\nbar\n/\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "bye\n");

    // A failing child fails the script like any other command.
    let err = sandbox.execute("tc exit 4").await.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ScriptFailed {
                exit_code: Some(4),
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn unregistered_commands_exit_127() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();

    let out = sandbox
        .execute("no-such-command; echo \"status=$?\"")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "status=127\n");
}

/// Without a VFS, a child has no filesystem at all: not even the host's.
#[tokio::test]
async fn children_cannot_read_host_files() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();

    let out = sandbox
        .execute("tc cat /etc/passwd; echo \"status=$?\"")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "error: NotFound\nstatus=1\n");
}

#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn children_share_the_sessions_vfs() {
    use eryx::vfs::{ArcStorage, InMemoryStorage};

    let executor = shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();

    let out = session
        .execute(
            r#"
echo from-shell > /data/a.txt
tc cat /data/a.txt
tc write /data/b.txt from-child
read -r b < /data/b.txt
echo "b=$b"
tc cat /etc/passwd; echo "status=$?"
"#,
        )
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-shell\nb=from-child\nerror: NotFound\nstatus=1\n"
    );
    use eryx::vfs::VfsStorage;
    assert_eq!(storage.read("/data/b.txt").await.unwrap(), b"from-child");
}

/// A precompiled command loads in the eryx that built it, and one stamped by
/// another eryx version fails with `IncompatiblePrecompiled`, as runtimes do.
#[tokio::test]
#[allow(unsafe_code)]
async fn precompiled_commands_carry_the_version_stamp() {
    let wasm = test_cmd_wasm();
    let precompiled = eryx::Executor::precompile(&wasm).unwrap();
    // SAFETY: just precompiled by this eryx.
    let command = unsafe { Command::from_precompiled(&precompiled) }.unwrap();
    let sandbox = shell().with_command("upper", command).build().unwrap();
    let out = sandbox.execute("echo ok | upper").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "OK\n");

    // Forge another eryx version into the stored string (a length byte, then
    // the bytes), padded to keep the rest of the file in place.
    let version = format!(
        "eryx {} (wasmtime {})",
        env!("CARGO_PKG_VERSION"),
        wasmtime::ModuleVersionStrategy::default().as_str()
    );
    let mut needle = vec![u8::try_from(version.len()).unwrap()];
    needle.extend_from_slice(version.as_bytes());
    let at = precompiled
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
        .expect("the stamp is stored");
    let forged = "eryx 0.0.1 (wasmtime 1)";
    let mut replacement = vec![u8::try_from(forged.len()).unwrap()];
    replacement.extend_from_slice(forged.as_bytes());
    replacement.resize(needle.len(), 0);
    let mut stale = precompiled.clone();
    stale[at..at + needle.len()].copy_from_slice(&replacement);

    // SAFETY: the bytes are ours; only the version string differs.
    let err = unsafe { Command::from_precompiled(&stale) }.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::IncompatiblePrecompiled { .. }),
        "{err:?}"
    );
}
