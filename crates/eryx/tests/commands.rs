//! External commands for the shell guest (`eryx:process`): registered
//! [`Command`]s run as child processes of the embedded shell runtime.
//!
//! Most tests use a tiny command built by the test itself
//! (tests/fixtures/test-cmd, which needs `rustup target add wasm32-wasip2`),
//! the `p3_` ones the same command built for WASI 0.3 (`rustup target
//! add wasm32-wasip3`), and the `p1_` ones for WASI preview 1 (`rustup
//! target add wasm32-wasip1`), a core module eryx adapts into a component.
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
    WASM.get_or_init(|| build_test_cmd("wasm32-wasip2")).clone()
}

/// The test command built for WASI 0.3: a p3 command component.
fn test_cmd_p3() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    Command::from_bytes(WASM.get_or_init(|| build_test_cmd("wasm32-wasip3"))).unwrap()
}

/// The test command built for WASI preview 1: a core module, which
/// `Command::from_bytes` adapts into a WASI 0.2 command component.
fn test_cmd_p1() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    Command::from_bytes(WASM.get_or_init(|| build_test_cmd("wasm32-wasip1"))).unwrap()
}

fn build_test_cmd(target: &str) -> Vec<u8> {
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-cmd/Cargo.toml");
    let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("test-cmd");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--release", "--locked"])
        .args(["--target", target, "--manifest-path"])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("running cargo");
    assert!(
        status.success(),
        "building the test command failed; is the {target} target installed?"
    );
    std::fs::read(target_dir.join(format!("{target}/release/eryx-test-cmd.wasm"))).unwrap()
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

/// A child sees only the shell's filesystem, never the host's.
#[tokio::test]
async fn children_cannot_read_host_files() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();

    let out = sandbox
        .execute("tc cat /etc/passwd; echo \"status=$?\"")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "error: NotFound\nstatus=1\n");
}

/// A child gets the shell's own filesystem, even without `unstable-vfs`
/// (#968): `/dev`, a read-only root, and the shell's `/tmp`, so what a child
/// writes there lasts exactly as long as what the shell writes, across a
/// session's executions but not into another session.
#[tokio::test]
async fn children_share_the_shells_filesystem() {
    let executor = shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap()
        .executor();
    let mut session = SessionExecutor::new(executor.clone(), &[]).await.unwrap();
    let run = async |session: &mut SessionExecutor, script: &str| {
        let out = session.execute(script).run().await.unwrap();
        String::from_utf8(out.stdout).unwrap()
    };

    let out = run(
        &mut session,
        r#"
tc cat /dev/null; echo "null=$?"
echo from-shell > /tmp/shell.txt
tc cat /tmp/shell.txt
tc write /tmp/child.txt from-child
read -r c < /tmp/child.txt; echo "child=$c"
tc write /new.txt x; echo "root=$?"
"#,
    )
    .await;
    assert_eq!(
        out,
        "null=0\nfrom-shell\nchild=from-child\nerror: PermissionDenied\nroot=1\n"
    );

    // Both files survive into the session's next execution, for both.
    let out = run(
        &mut session,
        "tc cat /tmp/shell.txt; tc cat /tmp/child.txt; cat /tmp/child.txt; echo",
    )
    .await;
    assert_eq!(out, "from-shell\nfrom-childfrom-child\n");

    // Another session sees neither.
    let mut other = SessionExecutor::new(executor, &[]).await.unwrap();
    let out = run(
        &mut other,
        "tc cat /tmp/shell.txt; echo \"s=$?\"; tc cat /tmp/child.txt; echo \"c=$?\"",
    )
    .await;
    assert_eq!(out, "error: NotFound\ns=1\nerror: NotFound\nc=1\n");
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
    // In the script's stderr: the message only quotes its last line, which
    // in a pipeline may be another stage's.
    let stderr = String::from_utf8_lossy(err.stderr().unwrap_or_default()).into_owned();
    assert!(stderr.contains("upper: limit-exceeded"), "{err}\n{stderr}");

    // The same under a budget smaller than the live-child cap.
    let sandbox = shell()
        .with_command("upper", test_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_child_host_bytes(4 << 20))
        .build()
        .unwrap();
    let stages = [r#"upper "$BIG""#; 8].join(" | ");
    let script = format!("{BIG}\n{stages}");
    let err = sandbox.execute(&script).await.unwrap_err();
    let stderr = String::from_utf8_lossy(err.stderr().unwrap_or_default()).into_owned();
    assert!(stderr.contains("upper: limit-exceeded"), "{err}\n{stderr}");

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

/// A running child's argv and env count against `max_child_host_bytes` for
/// as long as it runs, not just until the shell waits on it: with three
/// children each holding 1 MiB running, a fourth doesn't fit a 3.5 MiB
/// budget, and once they've finished it does. Background children start
/// asynchronously, so they're given a second (they take milliseconds), and
/// their end is polled for (this shell has no `wait`).
#[cfg(feature = "coreutils")]
#[tokio::test(flavor = "multi_thread")]
async fn running_children_keep_their_arguments_charged() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_resource_limits(
            ResourceLimits::default()
                .with_max_child_host_bytes(7 << 19)
                .with_execution_timeout(Duration::from_secs(60)),
        )
        .build()
        .unwrap();
    let script = format!(
        "{BIG}\nfor i in 1 2 3; do X=\"$BIG\" sleep 8 & done\nsleep 1\n\
         X=\"$BIG\" tc argv0; echo \"running=$?\"\n\
         until [ \"$(X=\"$BIG\" tc argv0 2>&1)\" = tc ]; do sleep 0.1; done\necho finished"
    );
    let out = sandbox.execute(&script).await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "running=126\nfinished\n");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("tc: limit-exceeded"),
        "{out:?}"
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

/// A child's output pump waits for the next stage to catch up, so a fast
/// producer feeding slow stages doesn't fill the shell's memory with
/// in-guest pipe buffers.
#[tokio::test]
async fn a_fast_child_waits_for_slow_stages() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    let stages = ["tc alloc 1"; 10].join(" | ");
    let out = sandbox
        .execute(&format!("tc yes | {stages} | head -c 3"))
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "y\ny");
    let peak = out.stats.peak_memory_bytes.unwrap();
    assert!(peak < 16 << 20, "the shell's memory peaked at {peak} bytes");
}

/// A child writing more than a pipe's high-water mark into a stage that
/// brush runs as a task of its own (a command substitution, or a compound
/// stage) doesn't wait forever: the task runs alongside its reader.
#[tokio::test]
async fn large_output_reaches_substitutions_and_compound_stages() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_secs(10)),
        )
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
x=$(printf '%100000s' a | tc); echo ${#x}
x=$(tc yes | head -c 1000000); echo ${#x}
{ printf '%100000s' a | tc; } | tc | head -c 3; echo
for i in 1 2 3; do printf '%100000s' a | tc; done | wc -c
"#,
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "100000\n999999\n   \n300000\n");
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

/// `without_embedded_commands` leaves the coreutils out, keeping the
/// registered commands.
#[tokio::test]
async fn embedded_commands_can_be_left_out() {
    let sandbox = shell()
        .without_embedded_commands()
        .with_command("upper", test_cmd())
        .build()
        .unwrap();
    let out = sandbox
        .execute("echo hi | upper; cat /nope; echo \"cat=$?\"")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "HI\ncat=127\n");

    // A pool's sandboxes leave them out too.
    let builder = shell()
        .without_embedded_commands()
        .with_command("upper", test_cmd());
    let pool = eryx::SandboxPool::new(builder, eryx::PoolConfig::default().with_max_size(1))
        .await
        .unwrap();
    let sandbox = pool.acquire().await.unwrap();
    let out = sandbox.execute("cat /nope; echo \"cat=$?\"").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "cat=127\n");
}

/// The common script tools added in #897 run in the embedded shell.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn the_embedded_shell_runs_common_script_tools() {
    assert_script_outputs(&[
        ("seq 3 | tr '\\n' ' '", "1 2 3 "),
        ("echo a,b,c | cut -d, -f2", "b\n"),
        ("echo hello | tr a-z A-Z", "HELLO\n"),
        (
            "printf 'a\\na\\nb\\n' | uniq -c | tr -s ' '",
            " 2 a\n 1 b\n",
        ),
        ("basename /x/y.txt .txt; dirname /x/y.txt", "y\n/x\n"),
        ("sleep 0 && echo slept", "slept\n"),
        ("printf '1\\n2\\n' | paste -sd+", "1+2\n"),
        ("date -u -d @0 +%Y", "1970\n"),
        ("echo hi | base64", "aGkK\n"),
        // Only the lexical forms; coreutils_see_the_root_and_dev canonicalizes.
        ("realpath -m /a/../b", "/b\n"),
        ("readlink -m /a/../b/c", "/b/c\n"),
    ])
    .await;
}

/// The #897 tools that write files in the shell's `/tmp`.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn the_embedded_shell_runs_common_script_tools_on_files() {
    assert_script_outputs(&[
        ("echo hi | tee /tmp/teed; cat /tmp/teed", "hi\nhi\n"),
        (
            "printf 'a\\nb\\n' > /tmp/c1; printf 'b\\nc\\n' > /tmp/c2; comm -12 /tmp/c1 /tmp/c2",
            "b\n",
        ),
        ("mkdir /tmp/t && rmdir /tmp/t && echo gone", "gone\n"),
        ("realpath /tmp/../tmp", "/tmp\n"),
        // Directories carry their own times rather than the epoch (#973).
        (
            "mkdir /tmp/d && y=$(date -r /tmp/d +%Y) && [ \"$y\" != 1970 ] && echo dated",
            "dated\n",
        ),
        (
            "mkdir /tmp/e && touch -d @100000 /tmp/e && date -r /tmp/e +%s",
            "100000\n",
        ),
        // touch sets an existing file's times through a read-only descriptor.
        (
            "echo x > /tmp/f && touch -d @100000 /tmp/f && date -r /tmp/f +%s",
            "100000\n",
        ),
        // WASI has no file modes, so cp doesn't try to copy them (#982).
        (
            "mkdir -p /tmp/a/s && echo x > /tmp/a/f && touch -d @100000 /tmp/a/f; \
             cp -r /tmp/a /tmp/b 2>&1; echo r=$?; \
             cp -p /tmp/a/f /tmp/g 2>&1; echo p=$?; \
             cp -rp /tmp/a /tmp/c 2>&1; echo rp=$?; \
             cat /tmp/b/f /tmp/g /tmp/c/f; date -r /tmp/g +%s",
            "r=0\np=0\nrp=0\nx\nx\nx\n100000\n",
        ),
        // `touch -r` reads the reference file's times.
        (
            "echo x > /tmp/r && touch -d @100000 /tmp/r && touch -r /tmp/r /tmp/s && date -r /tmp/s +%s",
            "100000\n",
        ),
    ])
    .await;
}

/// Run each script in a new embedded shell sandbox and check its stdout,
/// reporting every mismatch at once.
#[cfg(feature = "coreutils")]
async fn assert_script_outputs(cases: &[(&str, &str)]) {
    let sandbox = shell().build().unwrap();
    let mut failures = Vec::new();
    for (script, expected) in cases {
        match sandbox.execute(script).await {
            Ok(out) if out.stdout_utf8().unwrap() == *expected => {}
            Ok(out) => failures.push(format!("{script}: got {:?}", out.stdout_utf8().unwrap())),
            Err(e) => failures.push(format!("{script}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// `sleep` waits on a host timer rather than spinning, and still stops at the
/// parent's timeout.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn a_sleeping_child_stops_at_the_parents_timeout() {
    let sandbox = shell()
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let start = Instant::now();
    let err = sandbox.execute("sleep 60").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
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

/// `ls -R` descends nested directories on the shell's `/tmp` and, with
/// `unstable-vfs`, the session's VFS and a host volume, and `cp -r` copies
/// them whole: uutils tells directories apart by device and inode on WASI, not
/// by type and size, which made every empty directory look already listed
/// (#972).
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn coreutils_walk_nested_directories() {
    let executor = shell().build().unwrap().executor();
    #[cfg(not(feature = "unstable-vfs"))]
    let (mut session, roots) = (
        SessionExecutor::new(executor, &[]).await.unwrap(),
        ["/tmp"].as_slice(),
    );
    #[cfg(feature = "unstable-vfs")]
    let host = tempfile::tempdir().unwrap();
    #[cfg(feature = "unstable-vfs")]
    let (mut session, roots) = {
        use eryx::session::{VfsConfig, VolumeMount};
        use eryx::vfs::{ArcStorage, InMemoryStorage};
        use std::sync::Arc;

        let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
        let session = SessionExecutor::new_with_vfs_config(
            executor,
            &[],
            storage,
            VfsConfig::default().with_volumes([VolumeMount::new(host.path(), "/vol")]),
        )
        .await
        .unwrap();
        (session, ["/data", "/tmp", "/vol"].as_slice())
    };

    for root in roots {
        let out = session
            .execute(format!(
                "mkdir -p {root}/t/a/b {root}/t/c; touch {root}/t/x {root}/t/a/y {root}/t/a/b/z\n\
                 cp -r {root}/t {root}/u\n\
                 ls -R {root}/t; ls -R {root}/u"
            ))
            .run()
            .await
            .unwrap();
        let listing =
            |dir: &str| format!("{dir}:\na\nc\nx\n\n{dir}/a:\nb\ny\n\n{dir}/a/b:\nz\n\n{dir}/c:\n");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!(
                "{}{}",
                listing(&format!("{root}/t")),
                listing(&format!("{root}/u"))
            ),
            "{root}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Children see the shell's root and `/dev` too (#968), so tools that walk up
/// to `/` work.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn coreutils_see_the_root_and_dev() {
    let root = if cfg!(feature = "unstable-vfs") {
        "data dev tmp "
    } else {
        "dev tmp "
    };
    assert_script_outputs(&[
        ("ls / | tr '\\n' ' '", root),
        (
            "mkdir -p /tmp/a/b/c && realpath /tmp/a/../a/b && readlink -f /tmp/a/b/c",
            "/tmp/a/b\n/tmp/a/b/c\n",
        ),
        // One big read is served in bounded chunks.
        ("head -c 50000000 /dev/urandom | wc -c", "50000000\n"),
        ("head -c 1000 /dev/zero | tr -d '\\000' | wc -c", "0\n"),
        ("echo gone > /dev/null && cat /dev/null | wc -c", "0\n"),
    ])
    .await;

    // Nothing above the mounts can be changed, and outside them is empty.
    let sandbox = shell().build().unwrap();
    for (script, message) in [
        ("mkdir /etc", "mkdir: Permission denied"),
        ("mv /tmp /tmp2", "mv: Operation not permitted"),
        ("rm /dev/null", "Permission denied"),
        ("touch /new", "Permission denied"),
        ("cat /etc/passwd", "No such file or directory"),
    ] {
        let err = sandbox.execute(script).await.unwrap_err();
        assert!(err.to_string().contains(message), "{script}: {err}");
    }
}

/// A WASI 0.3 command runs as a child like a 0.2 one: argv, env, stdin,
/// stdout, stderr and exit status, and pipes to and from 0.2 stages.
#[tokio::test]
async fn p3_commands_run_as_children() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_commands(["tc3", "other3"], test_cmd_p3())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "tc3 argv0; other3 argv0; FOO=bar tc3 env FOO; tc3 exit 3; echo \"status=$?\"; \
             printf 'hi\\n' | tc3 | tc; echo lo | tc | tc3",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "tc3\nother3\nbar\nstatus=3\nHI\nLO\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "bye\n");
}

/// A WASI 0.3 child streams through its pipes: an endless producer stops
/// at `head`, and bulk and line-at-a-time output pass through.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn p3_children_stream_through_pipes() {
    let sandbox = shell()
        .with_command("tc3", test_cmd_p3())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_secs(30)),
        )
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "tc3 yes | head -n 2; tc3 yes | head -c 20000000 | wc -c; \
             tc3 lines | cat | head -c 100000 | wc -c; x=$(tc3 yes | head -c 1000000); echo ${#x}",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "y\ny\n20000000\n100000\n999999\n"
    );
}

/// A WASI 0.3 child burns the parent's fuel, stops at its timeout and
/// cancellation, and draws on the children's memory pool.
#[tokio::test(flavor = "multi_thread")]
async fn p3_children_are_limited_like_p2_ones() {
    let sandbox = shell()
        .with_command("tc3", test_cmd_p3())
        .with_resource_limits(ResourceLimits::default().with_max_fuel(100_000_000))
        .build()
        .unwrap();
    sandbox.execute("tc3 argv0").await.unwrap();
    let err = sandbox.execute("tc3 spin").await.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );

    let sandbox = shell()
        .with_command("tc3", test_cmd_p3())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let err = sandbox.execute("tc3 spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");

    let sandbox = shell().with_command("tc3", test_cmd_p3()).build().unwrap();
    let handle = sandbox.execute_cancellable("tc3 spin");
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.cancel();
    let err = tokio::time::timeout(Duration::from_secs(3), handle.wait())
        .await
        .expect("cancellation stops the child")
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Cancelled), "{err:?}");

    // 160 MiB fits the 256 MiB pool alone, not twice over.
    let out = sandbox.execute("echo hi | tc3 alloc 160").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "hi\n");
    let out = sandbox
        .execute("echo hi | tc3 alloc 160 | tc3 alloc 160; true")
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error: OutOfMemory"),
        "{out:?}"
    );
}

/// A WASI 0.3 child sees the session's VFS, as 0.2 children do, and no
/// host files.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn p3_children_share_the_sessions_vfs() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let executor = shell()
        .with_command("tc", test_cmd())
        .with_command("tc3", test_cmd_p3())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();
    let out = session
        .execute(
            "echo from-shell > /data/a.txt; tc3 cat /data/a.txt; \
             tc3 write /data/b.txt from-p3; tc cat /data/b.txt; echo; \
             tc3 cat /etc/passwd; echo \"status=$?\"",
        )
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-shell\nfrom-p3\nerror: NotFound\nstatus=1\n"
    );
    assert_eq!(storage.read("/data/b.txt").await.unwrap(), b"from-p3");
}

/// A WASI preview 1 command module runs as a child once adapted: argv, env,
/// stdin, stdout, stderr and exit status, pipes to and from components, and
/// `eryx:process/run` when allowed to spawn. Wasmtime's adapter passes on
/// only success or failure, so `exit 3` exits 1.
#[tokio::test]
async fn p1_commands_run_as_children() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_commands(["tc1", "other1"], test_cmd_p1())
        .with_command("spawner1", test_cmd_p1().allow_spawn())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "tc1 argv0; other1 argv0; FOO=bar tc1 env FOO; tc1 exit 3; echo \"status=$?\"; \
             printf 'hi\\n' | tc1 | tc; echo lo | tc | tc1; echo sp | spawner1 spawn tc",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "tc1\nother1\nbar\nstatus=1\nHI\nLO\nSP\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "bye\n");
}

/// A WASI preview 1 child sees the session's VFS through the adapter, and
/// no host files.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn p1_children_share_the_sessions_vfs() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let executor = shell()
        .with_command("tc", test_cmd())
        .with_command("tc1", test_cmd_p1())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();
    let out = session
        .execute(
            "echo from-shell > /data/a.txt; tc1 cat /data/a.txt; \
             tc1 write /data/b.txt from-p1; tc cat /data/b.txt; echo; \
             tc1 cat /etc/passwd; echo \"status=$?\"",
        )
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-shell\nfrom-p1\nerror: NotFound\nstatus=1\n"
    );
    assert_eq!(storage.read("/data/b.txt").await.unwrap(), b"from-p1");
}

/// The test command, allowed to run commands itself (`tc spawn CMD ...`).
fn spawning_cmd() -> Command {
    test_cmd().allow_spawn()
}

/// What a script printed, to stdout and stderr, whether or not it failed.
async fn run_script(sandbox: &Sandbox, script: &str) -> String {
    let (stdout, stderr) = match sandbox.execute(script).await {
        Ok(out) => (out.stdout, out.stderr),
        Err(e) => match e.kind() {
            ErrorKind::ScriptFailed { stdout, stderr, .. } => (stdout.clone(), stderr.clone()),
            _ => panic!("{e}"),
        },
    };
    format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    )
}

/// A child whose command may spawn runs commands through
/// `eryx:process/run`: with its stdin, args, env and exit status, from WASI
/// 0.2 and 0.3 children alike, and only commands that are registered.
#[tokio::test]
async fn children_run_commands_of_their_own() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_command("tc3", test_cmd_p3().allow_spawn())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "echo hi | tc spawn tc; tc spawn tc exit 3; echo \"status=$?\"; \
             FOO=bar tc spawn tc env FOO; tc spawn tc argv0; \
             echo lo | tc3 spawn tc; echo p3 | tc spawn tc3; tc spawn nope; true",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "HI\nstatus=3\nbar\ntc\nLO\nP3\nerror: command-not-found\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "bye\n");
}

/// Only commands registered with `allow_spawn` may run others, at any depth.
#[tokio::test]
async fn only_commands_allowed_to_spawn_can() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_command("plain", test_cmd())
        .build()
        .unwrap();
    let out = run_script(
        &sandbox,
        "plain spawn tc argv0; tc spawn plain spawn tc argv0",
    )
    .await;
    assert_eq!(out, "error: spawn-failed\nerror: spawn-failed\n");
}

/// Children nest at most four deep (the shell's are the first).
#[tokio::test]
async fn nested_spawns_stop_at_the_depth_limit() {
    let sandbox = shell().with_command("tc", spawning_cmd()).build().unwrap();
    let out = run_script(&sandbox, "tc spawn tc spawn tc spawn tc argv0").await;
    assert_eq!(out, "tc\n");
    let out = run_script(&sandbox, "tc spawn tc spawn tc spawn tc spawn tc argv0").await;
    assert_eq!(out, "error: limit-exceeded\n");
}

/// A nested child burns its execution's fuel, stops at its timeout, and
/// draws on the children's memory pool.
#[tokio::test(flavor = "multi_thread")]
async fn nested_children_share_the_executions_limits() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_fuel(100_000_000))
        .build()
        .unwrap();
    sandbox.execute("tc spawn tc argv0").await.unwrap();
    let err = sandbox
        .execute("tc spawn tc spawn tc spin")
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );

    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let start = Instant::now();
    let err = sandbox.execute("tc spawn tc spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );

    // The pool is 256 MiB.
    let sandbox = shell().with_command("tc", spawning_cmd()).build().unwrap();
    let out = run_script(&sandbox, "echo hi | tc spawn tc alloc 160").await;
    assert_eq!(out, "hi\n");
    let out = run_script(&sandbox, "echo hi | tc spawn tc alloc 300").await;
    assert_eq!(out, "error: OutOfMemory\n");
}

/// A nested run's output is capped: a command writing more is stopped.
/// (`run` returns output only once the command exits.)
#[tokio::test]
async fn nested_output_is_capped() {
    let sandbox = shell().with_command("tc", spawning_cmd()).build().unwrap();
    let out = run_script(&sandbox, "tc spawn tc yes").await;
    assert_eq!(out, "error: limit-exceeded\n");
}

/// A command piped through `eryx:process/pipe` gets its caller's input as
/// it's written and gives back its output, chained through more piped
/// commands; its stderr is its caller's, and `wait` returns its status. It
/// needs `allow_spawn`, and nests at most as deep as `run`.
#[tokio::test]
async fn piped_children_stream_both_ways() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_command("plain", test_cmd())
        .build()
        .unwrap();
    let out = run_script(
        &sandbox,
        "echo hi | tc pipe tc copy; echo lo | tc pipe tc pipe tc copy; \
         tc pipe tc exit 3; echo \"status=$?\"; tc pipe nope; plain pipe tc argv0; true",
    )
    .await;
    assert_eq!(
        out, "HI\nLO\nstatus=3\nerror: command-not-found\nerror: spawn-failed\nbye\n",
        "{out}"
    );
    let out = run_script(&sandbox, "tc pipe tc pipe tc pipe tc argv0").await;
    assert_eq!(out, "tc\n");
    let out = run_script(&sandbox, "tc pipe tc pipe tc pipe tc pipe tc argv0").await;
    assert_eq!(out, "error: limit-exceeded\n");
}

/// Much more than the pipes hold flows through a piped command, a chunk at
/// a time, with each side waiting on the other; one that's written without
/// being read stalls, as with any pipe, until the timeout stops it.
#[tokio::test(flavor = "multi_thread")]
async fn piped_children_apply_backpressure() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(500)),
        )
        .build()
        .unwrap();
    let out = run_script(&sandbox, "tc pipe-echo 1048576 tc copy").await;
    assert_eq!(out, "echoed 1048576\n");

    let start = Instant::now();
    let err = sandbox
        .execute("tc pipe-flood 1048576 tc copy")
        .await
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

/// Piped commands hold live-child slots, so their number is capped, and a
/// caller exiting without waiting for them kills them and frees their
/// slots at once.
#[tokio::test(flavor = "multi_thread")]
async fn piped_children_are_capped_and_killed_with_their_caller() {
    let sandbox = shell().with_command("tc", spawning_cmd()).build().unwrap();
    let out = run_script(&sandbox, "tc pipe-leak 100 tc spin").await;
    let (piped, error) = out
        .trim()
        .strip_prefix("piped ")
        .and_then(|rest| rest.split_once(' '))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(piped.parse::<usize>().unwrap() < 64, "{out}");
    assert_eq!(error, "error: limit-exceeded", "{out}");

    // 25 callers leaking 10 each: more than the 64 slots, unless each
    // caller's exit gives them back.
    let start = Instant::now();
    let out = run_script(
        &sandbox,
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25; do \
         tc pipe-leak 10 tc spin; done",
    )
    .await;
    assert_eq!(out, "piped 10\n".repeat(25), "{out}");
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
}

/// A piped command burns its execution's fuel, stops at its timeout, and
/// reports trace events, killed with its caller.
#[tokio::test(flavor = "multi_thread")]
async fn piped_children_share_the_executions_limits() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_resource_limits(ResourceLimits::default().with_max_fuel(100_000_000))
        .build()
        .unwrap();
    sandbox.execute("tc pipe tc argv0").await.unwrap();
    let err = sandbox.execute("tc pipe tc spin").await.unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );

    let handler = CollectTrace::default();
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_trace_handler(handler.clone())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let start = Instant::now();
    let err = sandbox.execute("tc pipe tc spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    let mut events = process_events(&handler.0.lock().unwrap());
    events.sort_by_key(|&(event, _, pid, ..)| (pid, event == "end"));
    let killed = |pid| {
        [
            ("start", "tc".to_string(), pid, None, None),
            (
                "end",
                "tc".to_string(),
                pid,
                None,
                Some("killed".to_string()),
            ),
        ]
    };
    assert_eq!(events, [killed(1), killed(2)].concat());
}

/// A session with a VFS whose children may spawn, and its storage.
#[cfg(feature = "unstable-vfs")]
async fn spawning_session() -> (SessionExecutor, eryx::vfs::ArcStorage) {
    use eryx::vfs::{ArcStorage, InMemoryStorage};
    use std::sync::Arc;

    let executor = shell()
        .with_command("tc", spawning_cmd())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();
    (session, storage)
}

/// The rounds `tc burn PATH N` recorded at `path`, 0 if none.
#[cfg(feature = "unstable-vfs")]
async fn rounds(storage: &eryx::vfs::ArcStorage, path: &str) -> u64 {
    use eryx::vfs::VfsStorage;
    match storage.read(path).await {
        Ok(data) => String::from_utf8_lossy(&data).trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// A piped command that has exited keeps its live-child slot while its
/// handle is kept (with its unread output), so kept handles hit the cap;
/// waiting on one again returns its status again.
#[tokio::test]
async fn kept_piped_children_keep_their_slots() {
    let sandbox = shell().with_command("tc", spawning_cmd()).build().unwrap();
    let out = run_script(&sandbox, "tc pipe-keep 100 tc exit 3").await;
    let (kept, rest) = out
        .strip_prefix("kept ")
        .and_then(|rest| rest.split_once(' '))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(kept.parse::<usize>().unwrap() < 64, "{out}");
    assert!(
        rest.starts_with("error: limit-exceeded\nagain Some(Ok(3))\n"),
        "{out}"
    );
}

/// A caller killed while it waits for a piped command, by running out of
/// fuel or by the timeout, takes the command with it: it makes no progress
/// afterwards.
#[cfg(feature = "unstable-vfs")]
#[tokio::test(flavor = "multi_thread")]
async fn piped_children_die_with_a_caller_killed_in_wait() {
    let (mut session, storage) = spawning_session().await;
    let stays_put = async |path: &str| {
        let before = rounds(&storage, path).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(rounds(&storage, path).await, before, "{path}");
        before
    };

    // Fuel for a few rounds (a round is ~0.5 billion).
    session.set_fuel_limit(Some(2_500_000_000));
    let err = session
        .execute("tc pipe-flood 0 tc burn /data/f 1000")
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );
    assert!(
        stays_put("/data/f").await >= 1,
        "it ran while its caller waited"
    );

    session.set_fuel_limit(None);
    session.set_execution_timeout(Some(Duration::from_millis(300)));
    let err = session
        .execute("tc pipe-flood 0 tc burn /data/t 1000")
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    stays_put("/data/t").await;
}

/// Piped commands burn their execution's fuel as they run, wherever their
/// caller waits: twenty spinning while their caller waits on one stop
/// within the limit, at most a partial round each over it, not twenty
/// times over.
#[cfg(feature = "unstable-vfs")]
#[tokio::test(flavor = "multi_thread")]
async fn piped_children_burn_their_executions_fuel() {
    let (mut session, storage) = spawning_session().await;
    // What a round of `burn` costs the execution, through a piped command.
    session.set_fuel_limit(Some(u64::MAX / 2));
    let mut fuel = async |rounds: u32| {
        session
            .execute(format!("tc pipe tc burn /data/r {rounds}"))
            .run()
            .await
            .unwrap()
            .fuel_consumed
            .unwrap()
    };
    let (one, three) = (fuel(1).await, fuel(3).await);
    let round = (three - one) / 2;
    assert!(round > 0, "{one} {three}");

    // Room for ten rounds.
    session.set_fuel_limit(Some(one + 9 * round));
    let err = session
        .execute("mkdir -p /data/b; tc pipe-burn 20 1000 /data/b tc")
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "{err:?}"
    );
    let mut total = 0;
    for i in 0..20 {
        total += rounds(&storage, &format!("/data/b/{i}")).await;
    }
    // Ten rounds of fuel, and a round started by each before it ran out.
    assert!(total <= 10 + 20, "{total} rounds");
}

/// Nested children see the same VFS as the shell and their caller.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn nested_children_share_the_sessions_vfs() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let executor = shell()
        .with_command("tc", spawning_cmd())
        .build()
        .unwrap()
        .executor();
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage.clone())
        .await
        .unwrap();
    let out = session
        .execute(
            "echo from-shell > /data/a.txt; tc spawn tc cat /data/a.txt; \
             tc spawn tc write /data/b.txt nested; read -r b < /data/b.txt; echo \"b=$b\"",
        )
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-shell\nb=nested\n"
    );
    assert_eq!(storage.read("/data/b.txt").await.unwrap(), b"nested");
}

/// A line-at-a-time producer moves data at a useful rate (#890). Timed
/// against a buffered producer in the same test, so the bound follows the
/// runner's speed: 100 KB in 2-byte lines takes ~0.55x as long as 20 MB
/// from `yes` (debug and release alike), and took ~7x with #890's old
/// pipes (~100 µs per line). `pipe_throughput` has the real numbers.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn line_at_a_time_pipes_are_not_slow() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    let time = async |script: &str, bytes: &str| {
        let start = Instant::now();
        let out = sandbox.execute(script).await.unwrap();
        assert_eq!(out.stdout_utf8().unwrap().trim(), bytes);
        start.elapsed()
    };
    // Warm up, so neither timed run pays for the first instantiation.
    time("tc yes | head -c 100000 | wc -c", "100000").await;
    let buffered = time("tc yes | head -c 20000000 | wc -c", "20000000").await;
    let lines = time("tc lines | head -c 100000 | wc -c", "100000").await;
    let ratio = lines.as_secs_f64() / buffered.as_secs_f64();
    assert!(
        ratio < 3.0,
        "lines took {lines:?}, {ratio:.1}x the buffered run's {buffered:?}"
    );
}

/// Throughput of child pipes, for #890: line-at-a-time producers (one
/// write and flush per 2-byte line), a buffered one, and the first line's
/// latency. Prints timings; run it by name with `--run-ignored only`.
#[cfg(feature = "coreutils")]
#[tokio::test]
#[ignore = "benchmark"]
async fn pipe_throughput() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_command("tc3", test_cmd_p3())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_secs(120)),
        )
        .build()
        .unwrap();
    for (label, script, bytes) in [
        (
            "lines -> head",
            "tc lines | head -c 100000 | wc -c",
            100_000,
        ),
        (
            "lines -> cat -> head",
            "tc lines | cat | head -c 10000 | wc -c",
            10_000,
        ),
        (
            "yes -> head",
            "tc yes | head -c 20000000 | wc -c",
            20_000_000,
        ),
        // Latency: the first line should reach `head` long before the pipe fills.
        (
            "first lines -> cat -> head",
            "tc lines | cat | head -c 10 | wc -c",
            10,
        ),
        // The same, from a WASI 0.3 child.
        (
            "p3 lines -> head",
            "tc3 lines | head -c 100000 | wc -c",
            100_000,
        ),
        (
            "p3 lines -> cat -> head",
            "tc3 lines | cat | head -c 10000 | wc -c",
            10_000,
        ),
        (
            "p3 yes -> head",
            "tc3 yes | head -c 20000000 | wc -c",
            20_000_000,
        ),
        (
            "p3 first lines -> head",
            "tc3 lines | head -c 10 | wc -c",
            10,
        ),
        (
            "p3 first lines -> cat -> head",
            "tc3 lines | cat | head -c 10 | wc -c",
            10,
        ),
    ] {
        let start = Instant::now();
        let out = sandbox.execute(script).await.unwrap();
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(out.stdout_utf8().unwrap().trim(), bytes.to_string());
        eprintln!(
            "BENCH {label}: {bytes} bytes in {secs:.3}s = {:.0} KB/s",
            bytes as f64 / secs / 1000.0
        );
    }
}

/// A `process_start` or `process_end` trace event: `(event, name, pid,
/// exit_code, error)`.
type ProcessEvent = (&'static str, String, u32, Option<i32>, Option<String>);

/// The `process_start`/`process_end` trace events of `events`.
fn process_events(events: &[eryx::TraceEvent]) -> Vec<ProcessEvent> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            eryx::TraceEventKind::ProcessStart { name, pid, .. } => {
                Some(("start", name.clone(), *pid, None, None))
            }
            eryx::TraceEventKind::ProcessEnd {
                name,
                pid,
                exit_code,
                error,
                ..
            } => Some(("end", name.clone(), *pid, *exit_code, error.clone())),
            _ => None,
        })
        .collect()
}

/// Each child reports `process_start` (its arguments as context) and
/// `process_end` with its exit code, paired by a per-execution pid; secret
/// placeholders in arguments are scrubbed.
#[tokio::test]
async fn children_report_trace_events() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_secret("TOKEN", "real", vec![])
        .with_trace_collection(true)
        .build()
        .unwrap();

    let out = sandbox
        .execute("tc exit 3 || true; echo hi | tc | tc >/dev/null; tc env \"$TOKEN\" >/dev/null")
        .await
        .unwrap();
    let mut events = process_events(&out.trace);
    // Pipeline stages start and end in any order; pair them by pid.
    events.sort_by_key(|&(event, _, pid, ..)| (pid, event == "end"));
    let expected = |pid, exit_code| {
        [
            ("start", "tc".to_string(), pid, None, None),
            ("end", "tc".to_string(), pid, Some(exit_code), None),
        ]
    };
    assert_eq!(
        events,
        [
            expected(1, 3),
            expected(2, 0),
            expected(3, 0),
            expected(4, 0)
        ]
        .concat(),
        "{:?}",
        out.trace
    );

    let args: Vec<_> = out
        .trace
        .iter()
        .filter(|e| matches!(e.event, eryx::TraceEventKind::ProcessStart { .. }))
        .map(|e| e.context.clone().unwrap())
        .collect();
    assert_eq!(args[0], serde_json::json!({ "args": ["exit", "3"] }));
    let last = args[3].to_string();
    assert!(
        last.contains("[REDACTED]") && !last.contains("real"),
        "{last}"
    );
}

/// Collects the trace events a sandbox's handler receives.
#[derive(Clone, Default)]
struct CollectTrace(std::sync::Arc<std::sync::Mutex<Vec<eryx::TraceEvent>>>);

#[async_trait::async_trait]
impl eryx::TraceHandler for CollectTrace {
    async fn on_trace(&self, event: eryx::TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// A child the parent's timeout kills still reports its `process_end`, to a
/// trace handler (a failed execution returns no collected trace).
#[tokio::test]
async fn a_child_killed_by_the_timeout_reports_its_end() {
    let handler = CollectTrace::default();
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_trace_handler(handler.clone())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let err = sandbox.execute("tc spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");

    let events = handler.0.lock().unwrap().clone();
    assert_eq!(
        process_events(&events),
        [
            ("start", "tc".to_string(), 1, None, None),
            ("end", "tc".to_string(), 1, None, Some("killed".to_string())),
        ],
        "{events:?}"
    );
}

/// In a session, a timeout leaves the child's `wait` parked in the store
/// rather than dropping it; the execution still ends, and reports the child
/// killed. The next execution numbers its children afresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_a_child_its_timeout_killed() {
    use eryx::session::{InProcessSession, Session};

    let handler = CollectTrace::default();
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_trace_handler(handler.clone())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();

    let err = tokio::time::timeout(Duration::from_secs(10), session.execute("tc spin"))
        .await
        .expect("the timed-out execution never finished")
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    let events = std::mem::take(&mut *handler.0.lock().unwrap());
    assert_eq!(
        process_events(&events),
        [
            ("start", "tc".to_string(), 1, None, None),
            ("end", "tc".to_string(), 1, None, Some("killed".to_string())),
        ],
        "{events:?}"
    );

    session.reset().await.unwrap();
    session.execute("tc exit 0").await.unwrap();
    let events = handler.0.lock().unwrap().clone();
    assert_eq!(
        process_events(&events),
        [
            ("start", "tc".to_string(), 1, None, None),
            ("end", "tc".to_string(), 1, Some(0), None),
        ],
        "{events:?}"
    );
}

/// Nested children (see `Command::allow_spawn`) report their process events
/// too, numbered in the same execution, and are reported killed with their
/// caller when the timeout stops them.
#[tokio::test]
async fn nested_children_report_trace_events() {
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_trace_collection(true)
        .build()
        .unwrap();
    let out = sandbox
        .execute("tc spawn tc spawn tc exit 3 || true")
        .await
        .unwrap();
    let mut events = process_events(&out.trace);
    events.sort_by_key(|&(event, _, pid, ..)| (pid, event == "end"));
    let pair = |pid| {
        [
            ("start", "tc".to_string(), pid, None, None),
            ("end", "tc".to_string(), pid, Some(3), None),
        ]
    };
    assert_eq!(
        events,
        [pair(1), pair(2), pair(3)].concat(),
        "{:?}",
        out.trace
    );

    let handler = CollectTrace::default();
    let sandbox = shell()
        .with_command("tc", spawning_cmd())
        .with_trace_handler(handler.clone())
        .with_resource_limits(
            ResourceLimits::default().with_execution_timeout(Duration::from_millis(300)),
        )
        .build()
        .unwrap();
    let err = sandbox.execute("tc spawn tc spin").await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    let mut events = process_events(&handler.0.lock().unwrap());
    events.sort_by_key(|&(event, _, pid, ..)| (pid, event == "end"));
    let killed = |pid| {
        [
            ("start", "tc".to_string(), pid, None, None),
            (
                "end",
                "tc".to_string(),
                pid,
                None,
                Some("killed".to_string()),
            ),
        ]
    };
    assert_eq!(events, [killed(1), killed(2)].concat());
}

/// `with_allowed_commands` narrows one execution's commands: the guest sees
/// only those, others exit 127, an empty list allows none, and names that
/// aren't registered are ignored. The next execution without it gets them
/// all back.
#[tokio::test]
async fn allowed_commands_narrow_one_session_execution() {
    let executor = shell()
        .with_command("tc", test_cmd())
        .with_command("upper", test_cmd())
        .build()
        .unwrap()
        .executor();
    let mut session = SessionExecutor::new_with_limits(executor, &[], &ResourceLimits::default())
        .await
        .unwrap();
    let both = "upper argv0; echo \"upper=$?\"; tc argv0; echo \"tc=$?\"";

    let out = session
        .execute(both)
        .with_allowed_commands(["upper", "missing"])
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "upper\nupper=0\ntc=127\n"
    );

    let out = session.execute(both).run().await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "upper\nupper=0\ntc\ntc=0\n"
    );

    let out = session
        .execute(both)
        .with_allowed_commands(Vec::<String>::new())
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "upper=127\ntc=127\n");
}

/// The one-shot executor applies `with_allowed_commands` the same way.
#[tokio::test]
async fn allowed_commands_narrow_a_one_shot_execution() {
    let executor = shell()
        .with_command("tc", test_cmd())
        .with_command("upper", test_cmd())
        .build()
        .unwrap()
        .executor();
    let out = executor
        .execute("tc argv0; echo \"tc=$?\"; upper argv0; echo \"upper=$?\"")
        .with_allowed_commands(["tc"])
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "tc\ntc=0\nupper=127\n"
    );
}

/// `Sandbox::execute_with` narrows one call's commands, and a later plain
/// `execute` gets them all back.
#[tokio::test]
async fn execute_with_narrows_a_sandbox_call() {
    let sandbox = shell()
        .with_command("tc", test_cmd())
        .with_command("upper", test_cmd())
        .build()
        .unwrap();
    let both = "tc argv0; echo \"tc=$?\"; upper argv0; echo \"upper=$?\"";

    let options = eryx::ExecuteOptions::new().with_allowed_commands(["tc"]);
    let out = sandbox.execute_with(both, &options).await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "tc\ntc=0\nupper=127\n");

    let out = sandbox.execute(both).await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "tc\ntc=0\nupper\nupper=0\n");

    let outcome = sandbox
        .execute_journaled(
            both,
            None,
            &eryx::ExecuteOptions::new().with_allowed_commands(["upper"]),
        )
        .await;
    assert_eq!(
        outcome.result.unwrap().stdout_utf8().unwrap(),
        "tc=127\nupper\nupper=0\n"
    );
}

/// `ExecuteOptions::with_cancellation` stops a call when its token is cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn execute_with_cancels_on_its_token() {
    let sandbox = shell().with_command("tc", test_cmd()).build().unwrap();
    let token = eryx::CancellationToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });
    let options = eryx::ExecuteOptions::new().with_cancellation(token);
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        sandbox.execute_with("tc spin", &options),
    )
    .await
    .expect("cancelling didn't stop the call")
    .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Cancelled), "{err:?}");
}

/// Nested children are held to their execution's `with_allowed_commands`
/// too: a command it narrowed away can't be reached through one that may
/// spawn.
#[tokio::test]
async fn nested_spawns_respect_allowed_commands() {
    let executor = shell()
        .with_command("tc", spawning_cmd())
        .with_command("upper", test_cmd())
        .build()
        .unwrap()
        .executor();
    let out = executor
        .execute("echo hi | tc spawn upper; tc spawn tc argv0")
        .with_allowed_commands(["tc"])
        .run()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "error: command-not-found\ntc\n"
    );
    let out = executor
        .execute("echo hi | tc spawn upper")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "HI\n");
}

/// The shell's `grep` builtin counts as a command for `with_allowed_commands`:
/// left out, it exits 127; allowed, or with no list, it runs.
#[tokio::test]
async fn allowed_commands_cover_the_grep_builtin() {
    let executor = shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap()
        .executor();
    let mut session = SessionExecutor::new_with_limits(executor, &[], &ResourceLimits::default())
        .await
        .unwrap();
    let code = "echo hit | grep hit; echo \"grep=$?\"";

    let refused = session
        .execute(code)
        .with_allowed_commands(["tc"])
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&refused.stdout), "grep=127\n");
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "error: command not found: grep\n"
    );

    let allowed = session
        .execute(code)
        .with_allowed_commands(["grep"])
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&allowed.stdout), "hit\ngrep=0\n");

    let none = session
        .execute(code)
        .with_allowed_commands(Vec::<String>::new())
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&none.stdout), "grep=127\n");

    let all = session.execute(code).run().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&all.stdout), "hit\ngrep=0\n");
}

/// A child starts in the shell's directory: after `cd`, the embedded
/// coreutils (built by eryx-build, which links in the start-in-`$PWD`
/// constructor) resolve relative paths against it (#978).
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn children_start_in_the_shells_directory() {
    let out = shell()
        .build()
        .unwrap()
        .execute(
            "mkdir -p /tmp/d/sub && cd /tmp/d && echo hi > f && cat f && ls && cd sub && cat ../f",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "hi\nf\nsub\nhi\n");
}

/// Opt-in: C git runs `git` itself for `stash` (and aliases), through
/// `eryx:process/run`, when registered with `allow_spawn`. Set
/// ERYX_TEST_GIT_WASM to its component: eryx-org/eryx-commands' published
/// `eryx-gpl/git` (as CI pulls it), or a build of its recipe there. It works
/// on the session's VFS.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
#[ignore = "needs ERYX_TEST_GIT_WASM"]
async fn git_stash_runs_git_itself() {
    use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let git = std::env::var_os("ERYX_TEST_GIT_WASM").expect("ERYX_TEST_GIT_WASM");
    let executor = shell()
        .with_command("git", Command::from_file(git).unwrap().allow_spawn())
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
export HOME=/data GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=eryx GIT_AUTHOR_EMAIL=eryx@example.com
export GIT_COMMITTER_NAME=eryx GIT_COMMITTER_EMAIL=eryx@example.com
mkdir -p /data/repo && cd /data/repo
git init -q
echo one > f.txt
git add f.txt && git commit -q -m one
echo two > f.txt
git stash
echo "after stash: $(cat f.txt)"
git stash list
git stash pop
echo "after pop: $(cat f.txt)"
echo "list after pop: [$(git stash list)]"
git config alias.st "status --short"
git st
"#,
        )
        .run()
        .await;
    let out = match out {
        Ok(out) => out,
        Err(e) => panic!("{e}"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("after stash: one"),
        "{stdout}{:?}",
        out.stderr
    );
    assert!(stdout.contains("stash@{0}"), "{stdout}");
    assert!(stdout.contains("after pop: two"), "{stdout}");
    // A pop whose apply failed would keep the entry.
    assert!(stdout.contains("list after pop: []"), "{stdout}");
    // `git st` is an alias once running `git-st` fails as an exec does.
    assert!(stdout.ends_with(" M f.txt\n"), "{stdout}");
    assert_eq!(storage.read("/data/repo/f.txt").await.unwrap(), b"two\n");
}

/// Children run on their own on-demand engine (#833), so running ones take
/// none of the pooling allocator's slots: only the shell holds one.
#[cfg(feature = "coreutils")]
#[tokio::test]
async fn children_take_no_pool_slots() {
    let engine = eryx::Executor::shared_engine().unwrap();
    let Some(before) = engine.pooling_allocator_metrics() else {
        return; // On-demand (Windows, or ERYX_ALLOCATOR): there's no pool.
    };
    let (instances, memories) = (before.component_instances(), before.memories());
    let sandbox = shell().build().unwrap();
    let run = tokio::spawn(async move { sandbox.execute("sleep 1 | sleep 1 | sleep 1").await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let during = engine.pooling_allocator_metrics().unwrap();
    // The shell itself, and nothing for its three sleeping children.
    assert!(
        during.component_instances() <= instances + 1,
        "{}",
        during.component_instances()
    );
    assert!(during.memories() <= memories + 1, "{}", during.memories());
    run.await.unwrap().unwrap();
}
