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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use eryx::language::Shell;
use eryx::{Command, ErrorKind, ResourceLimits, Sandbox, SandboxBuilder, SessionExecutor, state};

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
    use std::sync::Arc;

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

/// Sets `$BIG` to 1 MiB.
const BIG: &str = "BIG=x; i=0; while [ $i -lt 20 ]; do BIG=$BIG$BIG; i=$((i + 1)); done";

/// The shell spawns every pipeline stage before running any, and children's
/// arguments live in host memory, so a long pipeline of large arguments must
/// hit the child limits and fail the spawn instead of growing the host.
#[tokio::test]
async fn a_pipeline_of_large_arguments_fails_the_spawn() {
    let sandbox = shell()
        .with_command("upper", test_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_memory_bytes(64 << 20))
        .build()
        .unwrap();

    let stages = vec![r#"upper "$BIG""#; 300].join(" | ");
    let script = format!("{BIG}\n{stages}");
    let err = sandbox.execute(&script).await.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::ScriptFailed { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("upper: limit-exceeded"), "{err}");

    // The same under a budget smaller than the live-child cap.
    let sandbox = shell()
        .with_command("upper", test_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_child_host_bytes(4 << 20))
        .build()
        .unwrap();
    let stages = [r#"upper "$BIG""#; 8].join(" | ");
    let script = format!("{BIG}\n{stages}");
    let err = sandbox.execute(&script).await.unwrap_err();
    assert!(err.to_string().contains("upper: limit-exceeded"), "{err}");

    // Within the budget, the same sandbox still runs children.
    let out = sandbox.execute("echo ok | upper | upper").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "OK\n");
}

/// A pool built from a shell builder keeps its registered commands.
#[tokio::test]
async fn pooled_sandboxes_keep_their_commands() {
    use eryx::{PoolConfig, SandboxPool};

    let builder = shell().with_command("upper", test_cmd());
    let config = PoolConfig::default().with_max_size(1).with_min_idle(1);
    let pool = SandboxPool::new(builder, config).await.unwrap();
    let sandbox = pool.acquire().await.unwrap();
    let out = sandbox.execute("echo hi | upper").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "HI\n");
}

/// A child burns the parent's fuel, so one that spins exhausts the
/// execution's limit rather than only its own.
#[tokio::test]
async fn children_burn_the_parents_fuel() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_fuel(100_000_000))
        .build()
        .unwrap();
    // The limit is plenty for the shell itself.
    sandbox.execute("tc argv0").await.unwrap();
    let err = sandbox.execute("tc spin").await.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );
}

/// A CPU-bound child stops at the parent's timeout, not when its own fuel
/// runs out (seconds later).
#[tokio::test]
async fn a_spinning_child_stops_at_the_parents_timeout() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let start = Instant::now();
    let err = sandbox.execute("tc spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

/// A child the timeout or cancellation drops midway leaves nothing behind
/// that stops the session running children afterwards, once it has reset
/// (after a timeout by hand; a cancel resets on the next call).
#[tokio::test(flavor = "multi_thread")]
async fn a_session_runs_children_after_one_is_dropped() {
    let executor = shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap()
        .executor();
    let limits = ResourceLimits::default()
        .with_execution_timeout(Duration::from_millis(300))
        .with_max_child_host_bytes(3 << 20);
    let mut session = SessionExecutor::new_with_limits(executor, &[], &limits)
        .await
        .unwrap();
    let spin = format!("{BIG}\nX=\"$BIG\" tc spin");

    for _ in 0..2 {
        let err = session.execute(&spin).run().await.unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
        session.reset(&[]).await.unwrap();
    }

    session.set_execution_timeout(None);
    let token = eryx::CancellationToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });
    let err = session
        .execute(&spin)
        .with_cancellation(token)
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Cancelled), "{err:?}");

    let out = session
        .execute(format!("{BIG}\nX=\"$BIG\" tc argv0; echo hi | tc"))
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tc\nHI\n");
    assert!(out.state_was_reset);
}

/// Cancelling the execution stops a CPU-bound child too.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_a_spinning_child() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    let handle = sandbox.execute_cancellable("tc spin");
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.cancel();
    let err = tokio::time::timeout(Duration::from_secs(3), handle.wait())
        .await
        .expect("cancellation stops the child")
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Cancelled), "{err:?}");
}

/// Pipeline stages stream into each other: an endless producer stops once
/// `head` has what it needs, and more output than any buffer passes through.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn pipelines_stream_between_children() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    let out = sandbox
        .execute("tc yes | head -n 2; tc yes | head -c 20000000 | wc -c")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "y\ny\n20000000\n");
}

/// Children in a pipeline run at once but share the parent's fuel: two
/// stages that each fit the limit alone end the execution with
/// `FuelExhausted` together, having done at most about one epoch tick of
/// work each past it.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn concurrent_children_share_the_fuel_limit() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let executor = shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();
    let fuel = async |session: &mut SessionExecutor, rounds: u32| {
        let out = session
            .execute(format!("tc burn /data/c {rounds}"))
            .run()
            .await
            .unwrap();
        out.fuel_consumed.unwrap()
    };
    let base = fuel(&mut session, 0).await;
    let round = (fuel(&mut session, 10).await - base) / 10;

    // Room for 30 rounds: each stage's 20 fit, both stages' 40 don't.
    let limit = base * 2 + round * 30;
    let err = session
        .execute("tc burn /data/a 20 | tc burn /data/b 20")
        .with_fuel_limit(limit)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );
    let mut done = 0;
    for path in ["/data/a", "/data/b"] {
        let rounds = storage.read(path).await.unwrap_or_default();
        done += String::from_utf8(rounds)
            .unwrap()
            .parse::<u64>()
            .unwrap_or(0);
    }
    // A round is longer than an epoch tick, so each stage gets at most one
    // round past the limit; without sharing, both would finish all 20.
    assert!(done <= 30 + 2, "{done} rounds done under a 30-round limit");
    assert!(done >= 20, "{done} rounds done under a 30-round limit");
}

/// Children whose work has no calls in it, which keep their fuel count in a
/// register, are charged as they run too: two spinning stages under a fuel
/// limit take about as long as one.
#[tokio::test]
async fn spinning_children_share_the_fuel_limit() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_fuel(2_000_000_000))
        .build()
        .unwrap();
    let time = async |script: &str| {
        let start = Instant::now();
        let err = sandbox.execute(script).await.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
            "{err:?}"
        );
        start.elapsed()
    };
    let one = time("tc spin").await;
    let two = time("tc spin | tc spin").await;
    let four = time("tc spin | tc spin | tc spin | tc spin").await;
    // Charged only at exit, each stage would burn the whole limit itself.
    assert!(two < one * 3 / 2, "one: {one:?}, two: {two:?}");
    assert!(four < one * 3 / 2, "one: {one:?}, four: {four:?}");
}

/// A child counts against the live-child limit until it finishes, not just
/// until it's waited on: background jobs that are still running hold their
/// slots, so the ones past the limit are refused.
#[tokio::test]
async fn running_children_count_as_live() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    // Each foreground `tc` yields, so every background job's child is waited
    // on (and was, before, no longer counted) before the next one spawns.
    let out = sandbox
        .execute("for i in {1..70}; do tc busy 50 & tc exit 0; done")
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("tc: limit-exceeded"),
        "{out:?}"
    );
}

/// Running children share one pool of memory: two that each fit it alone
/// can't both hold their memory at once.
#[tokio::test]
async fn concurrent_children_share_the_memory_pool() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    // 160 MiB each fits the 256 MiB pool alone.
    let out = sandbox.execute("echo hi | tc alloc 160").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "hi\n");
    let out = sandbox
        .execute("tc yes | tc alloc 160 | tc alloc 160 | head -c 2; true")
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error: OutOfMemory"),
        "{out:?}"
    );
    // Both gave their memory back.
    let out = sandbox.execute("echo hi | tc alloc 160").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "hi\n");
}

/// The embedded shell runs the embedded uutils coreutils (the `coreutils`
/// feature), and a registered command of the same name replaces one.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn the_embedded_shell_runs_coreutils() {
    let sandbox = shell().build().unwrap();
    let out = sandbox
        .execute("echo -e 'pear\\napple\\nfig' | sort | head -n 2; echo hello | wc -c")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "apple\nfig\n6\n");

    // Child exit codes propagate.
    let err = sandbox.execute("head --no-such-flag").await.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ScriptFailed {
                exit_code: Some(1),
                ..
            }
        ),
        "{err:?}"
    );

    let sandbox = shell().with_command("cat", test_cmd()).build().unwrap();
    let out = sandbox
        .execute("echo hi | cat; echo b | sort")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "HI\nb\n");
}

#[cfg(all(feature = "coreutils", feature = "unstable-vfs"))]
#[tokio::test]
async fn coreutils_work_on_the_sessions_vfs() {
    use eryx::vfs::{ArcStorage, InMemoryStorage};
    use std::sync::Arc;

    let executor = shell().build().unwrap().executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .unwrap();

    let out = session
        .execute(
            r#"
echo -e "pear\napple\nfig" > /data/fruit.txt
sort /data/fruit.txt > /data/sorted.txt
mkdir /data/dir && cp /data/sorted.txt /data/dir/copy.txt
read -r first < /data/dir/copy.txt
echo "first=$first"
ls /data/dir
wc -l < /data/fruit.txt
"#,
        )
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "first=apple\ncopy.txt\n3\n"
    );

    // Children can't reach past the mounts.
    let err = session.execute("cat /etc/passwd").run().await.unwrap_err();
    assert!(err.to_string().starts_with("exit status 1"), "{err}");
}
