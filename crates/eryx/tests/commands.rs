//! External commands for the shell guest (`eryx:process`): registered
//! [`Command`]s run as child processes of the embedded shell runtime.
//!
//! Most tests use a tiny command built by the test itself
//! (tests/fixtures/test-cmd, which needs `rustup target add wasm32-wasip2`),
//! and the `p3_` ones the same command built for WASI 0.3 (`rustup target
//! add wasm32-wasip3`).
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
        // Without `unstable-vfs` children have no filesystem until #812, so
        // only the lexical forms; coreutils_see_the_root_and_dev canonicalizes.
        ("realpath -m /a/../b", "/b\n"),
        ("readlink -m /a/../b/c", "/b/c\n"),
    ])
    .await;
}

/// The #897 tools that write files, which need the VFS's `/tmp`.
#[cfg(all(feature = "coreutils", feature = "unstable-vfs"))]
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

/// Children see the shell's root and `/dev` too (with `unstable-vfs`; without
/// it they wait on #812), so tools that walk up to `/` work.
#[cfg(all(feature = "coreutils", feature = "unstable-vfs"))]
#[tokio::test]
async fn coreutils_see_the_root_and_dev() {
    assert_script_outputs(&[
        ("ls / | tr '\\n' ' '", "data dev tmp "),
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

/// A child the parent's timeout kills still reports its `process_end`, to a
/// trace handler (a failed execution returns no collected trace).
#[tokio::test]
async fn a_child_killed_by_the_timeout_reports_its_end() {
    #[derive(Clone, Default)]
    struct Collect(std::sync::Arc<std::sync::Mutex<Vec<eryx::TraceEvent>>>);
    #[async_trait::async_trait]
    impl eryx::TraceHandler for Collect {
        async fn on_trace(&self, event: eryx::TraceEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    let handler = Collect::default();
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
