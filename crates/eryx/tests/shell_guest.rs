//! End-to-end tests for the brush shell guest (`eryx-guest-shell`), run
//! through the same host as the CPython guest from the embedded, precompiled
//! `runtime-shell.cwasm`. External commands are in tests/commands.rs; with
//! none registered, an unknown command exits 127.
//!
//! ```sh
//! mise run precompile-runtime-shell
//! cargo nextest run -p eryx --features embedded,shell --test shell_guest
//! ```
#![cfg(all(feature = "embedded", feature = "shell"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eryx::language::Shell;
use eryx::{
    Callback, CallbackError, ErrorKind, Executor, Language, Sandbox, SandboxBuilder, Schema,
    SessionExecutor, state,
};
use serde_json::{Value, json};

/// A shell sandbox on the embedded runtime: no wasm file, no stdlib.
fn shell_sandbox() -> Option<SandboxBuilder<Shell, state::Has>> {
    Some(Sandbox::embedded().shell())
}

fn shell_executor() -> Option<Arc<Executor>> {
    let executor = shell_sandbox()?.build().unwrap().executor();
    assert_eq!(executor.language(), Language::Shell);
    Some(executor)
}

struct TestCallback {
    name: &'static str,
    calls: Arc<AtomicU32>,
    handler: fn(Value) -> Result<Value, CallbackError>,
    schema: Schema,
}

impl TestCallback {
    fn new(name: &'static str, handler: fn(Value) -> Result<Value, CallbackError>) -> Self {
        Self {
            name,
            calls: Arc::new(AtomicU32::new(0)),
            handler,
            schema: Schema::empty(),
        }
    }

    /// Give the callback a parameters schema (`{"type": "object", ...}`).
    fn with_schema(mut self, schema: Value) -> Self {
        self.schema = Schema::try_from_value(schema).unwrap();
        self
    }
}

impl Callback for TestCallback {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "test callback"
    }
    fn parameters_schema(&self) -> Schema {
        self.schema.clone()
    }
    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let handler = self.handler;
        Box::pin(async move { handler(args) })
    }
}

fn add(args: Value) -> Result<Value, CallbackError> {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(json!(a + b))
}

fn add_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
        "required": ["a", "b"]
    })
}

#[tokio::test]
async fn stdout_stderr_and_result_variable() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(
            r#"
greet() { echo "hello, $1"; }
greet world
echo oops >&2
for i in 1 2 3; do printf '%s,' "$i"; done; echo
result='{"sum": 3}'
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "hello, world\n1,2,3,\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    assert_eq!(out.result.as_deref(), Some("{\"sum\": 3}"));
}

#[tokio::test]
async fn nonzero_exit_is_a_script_failure_with_output() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let err = sandbox.execute("echo before; exit 3").await.unwrap_err();
    let ErrorKind::ScriptFailed {
        message, exit_code, ..
    } = err.kind()
    else {
        panic!("expected a script failure, got {err:?}");
    };
    assert_eq!(message, "exit status 3");
    assert_eq!(*exit_code, Some(3));
    assert_eq!(err.stdout(), Some(&b"before\n"[..]));

    // A missing command is a script failure too, with bash's status.
    let err = sandbox.execute("no-such-command").await.unwrap_err();
    assert!(err.to_string().starts_with("exit status 127"), "{err}");
    assert!(matches!(
        err.kind(),
        ErrorKind::ScriptFailed {
            exit_code: Some(127),
            ..
        }
    ));
}

#[tokio::test]
async fn callbacks_are_commands() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add).with_schema(add_schema()))
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
x=$(add --a 2 --b 3)
y=$(invoke add "{\"a\": $x, \"b\": 10}")
z=$(add --json '{"a": 1}' --b 1)
q=$(add --a=4 --b=6)
echo "$x $y $z $q"
if ! fail 2>/dev/null; then echo "fail failed"; fi
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "5 15 2 10\nfail failed\n");
}

/// Every callback command reports `callback_start` (its arguments as context)
/// and `callback_end`, in the CPython and QuickJS guests' shape; a failed call
/// too. The host fills in the duration.
#[tokio::test]
async fn callbacks_report_trace_events() {
    use eryx::TraceEventKind;

    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add).with_schema(add_schema()))
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .with_trace_collection(true)
        .build()
        .unwrap();

    let out = sandbox
        .execute("add --a 1 --b 2 >/dev/null; fail 2>/dev/null || true; echo plain")
        .await
        .unwrap();
    let events: Vec<_> = out
        .trace
        .iter()
        .map(|e| match &e.event {
            TraceEventKind::CallbackStart { name, .. } => format!("start {name}"),
            TraceEventKind::CallbackEnd { name, .. } => format!("end {name}"),
            other => panic!("unexpected trace event {other:?}"),
        })
        .collect();
    assert_eq!(
        events,
        ["start add", "end add", "start fail", "end fail"],
        "{:?}",
        out.trace
    );
    let contexts: Vec<_> = out.trace.iter().map(|e| e.context.clone()).collect();
    assert_eq!(contexts[0], Some(json!({ "a": 1, "b": 2 })));
    assert_eq!(contexts[1], None);
}

/// Exit codes and messages: 1 for a failed callback, 2 for bad usage, 127 for
/// `invoke` of an unknown name; `--help` describes the arguments.
#[tokio::test]
async fn callback_command_errors_and_help() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add).with_schema(add_schema()))
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
fail; echo "fail=$?"
add --a 1 --a 2; echo "twice=$?"
add --a one; echo "typed=$?"
invoke missing; echo "missing=$?"
add --help
"#,
        )
        .await
        .unwrap();

    let stdout = out.stdout_utf8().unwrap();
    assert!(
        stdout.starts_with("fail=1\ntwice=2\ntyped=2\nmissing=127\n"),
        "{stdout}"
    );
    assert!(stdout.contains("--a <integer> (required)"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in [
        "fail: ",
        "add: --a given more than once",
        "add: --a expects an integer, got 'one'",
        "invoke: missing: no such callback",
    ] {
        assert!(stderr.contains(line), "missing {line:?} in {stderr}");
    }
}

/// Flag values are typed by the schema, and the result prints as text: a
/// string raw, `null` as nothing, anything else as compact JSON.
#[tokio::test]
async fn callback_arguments_follow_the_schema() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let echo = TestCallback::new("echo_args", Ok).with_schema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "verbose": {"type": "boolean"}
        }
    }));
    let greet = TestCallback::new("greet", |args| {
        Ok(json!(format!(
            "hi {}",
            args["name"].as_str().unwrap_or("?")
        )))
    })
    .with_schema(json!({"type": "object", "properties": {"name": {"type": "string"}}}));
    let nothing = TestCallback::new("nothing", |_| Ok(Value::Null));
    let sandbox = builder
        .with_callback(echo)
        .with_callback(greet)
        .with_callback(nothing)
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            "echo_args --id 007 --tags a --tags 1 --verbose; greet --name London; nothing; echo end",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "{\"id\":\"007\",\"tags\":[\"a\",\"1\"],\"verbose\":true}\nhi London\nend\n"
    );
}

/// Positional arguments go to the host under `__eryx_args__`, typed by the
/// property they bind to, and the host binds them like the Python guest does.
#[tokio::test]
async fn callback_positionals_bind_on_the_host() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let echo = TestCallback::new("echo_args", Ok).with_schema(json!({
        "type": "object",
        "properties": {"id": {"type": "string"}, "n": {"type": "integer"}}
    }));
    let sandbox = builder
        .with_callback(TestCallback::new("add", add).with_schema(add_schema()))
        .with_callback(echo)
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
add 4 5
add 4 --b=6
echo_args 007 3
add 1 2 3; echo "extra=$?"
add 1 --a 2; echo "twice=$?"
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "9\n10\n{\"id\":\"007\",\"n\":3}\nextra=1\ntwice=1\n"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in [
        "takes 2 positional arguments but 3 were given",
        "got multiple values for argument 'a'",
    ] {
        assert!(stderr.contains(line), "missing {line:?} in {stderr}");
    }
}

/// Positionals bind in the order the schema declares its properties (#860),
/// not alphabetically.
#[tokio::test]
async fn callback_positionals_follow_declaration_order() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let echo = TestCallback::new("echo_args", Ok).with_schema(json!({
        "type": "object",
        "properties": {"zeta": {"type": "string"}, "alpha": {"type": "string"}}
    }));
    let sandbox = builder.with_callback(echo).build().unwrap();

    let out = sandbox.execute("echo_args first").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "{\"zeta\":\"first\"}\n");
}

#[tokio::test]
async fn session_state_persists_and_snapshots_round_trip() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(Arc::clone(&executor), &[])
        .await
        .unwrap();

    session
        .execute("count=1; bump() { count=$((count + 1)); }; alias hi='echo hi'")
        .run()
        .await
        .unwrap();
    let out = session
        .execute("bump; bump; echo $count")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"3\n");

    // The result variable is consumed per execution.
    let out = session.execute("result=first").run().await.unwrap();
    assert_eq!(out.result.as_deref(), Some("\"first\""));
    let out = session.execute("true").run().await.unwrap();
    assert_eq!(out.result, None);

    let snapshot = session.snapshot_state().await.unwrap();
    eprintln!("shell snapshot: {} bytes", snapshot.data().len());

    let mut fresh = SessionExecutor::new(executor, &[]).await.unwrap();
    fresh.restore_state(&snapshot).await.unwrap();
    let out = fresh.execute("bump; echo $count").run().await.unwrap();
    assert_eq!(out.stdout, b"4\n");

    fresh.clear_state().await.unwrap();
    let out = fresh.execute("echo \"[${count:-}]\"").run().await.unwrap();
    assert_eq!(out.stdout, b"[]\n");
}

/// A snapshot the WASI 0.2 build of the shell took (before the shell moved to
/// WASI 0.3) restores in this one: the target doesn't change brush's state
/// format. The fixture holds `count=2`, a `bump` function and an alias `hi`.
///
/// When `STATE_FORMAT` changes (e.g. a brush pin bump), restoring it fails by
/// design: delete the fixture and this test, or regenerate the fixture from a
/// WASI 0.2 build of the new guest with [`capture_wasi_0_2_snapshot`].
#[tokio::test]
async fn a_wasi_0_2_snapshot_restores() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/shell-p2-snapshot.bin"
    ))
    .unwrap();
    let snapshot = eryx::StateSnapshot::from_bytes(&bytes).unwrap();
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    session.restore_state(&snapshot).await.unwrap();
    let out = session
        .execute("bump; echo $count; shopt -s expand_aliases; alias hi")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"3\nalias hi='echo hi'\n");
}

/// Regenerates `tests/fixtures/shell-p2-snapshot.bin` from a WASI 0.2 build
/// of the shell, for [`a_wasi_0_2_snapshot_restores`]:
///
/// ```sh
/// mise run build-runtime-shell-p2
/// ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run -p eryx-precompile --release -- compile \
///   crates/eryx-runtime/runtime-shell-p2.wasm --language shell --target native \
///   --output /tmp/runtime-shell-p2.cwasm --no-verify
/// ERYX_RUNTIME_CWASM_SHELL=/tmp/runtime-shell-p2.cwasm ERYX_CAPTURE_SHELL_SNAPSHOT=1 \
///   cargo nextest run -p eryx --features embedded,shell --test shell_guest \
///   --run-ignored only capture_wasi_0_2_snapshot
/// ```
#[tokio::test]
#[ignore = "regenerates a fixture; see the doc comment"]
async fn capture_wasi_0_2_snapshot() {
    if std::env::var_os("ERYX_CAPTURE_SHELL_SNAPSHOT").is_none() {
        return;
    }
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    session
        .execute("count=1; bump() { count=$((count + 1)); }; alias hi='echo hi'; bump")
        .run()
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    std::fs::write(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/shell-p2-snapshot.bin"
        ),
        snapshot.to_bytes(),
    )
    .unwrap();
}

#[tokio::test]
async fn fuel_limit_stops_an_infinite_loop() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();

    let err = session
        .execute("while true; do :; done")
        .with_fuel_limit(50_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );
}

#[tokio::test]
async fn suspending_callback_halts_the_guest() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let approve = TestCallback::new("approve", |_| {
        Err(CallbackError::Suspend("needs human approval".to_string()))
    });
    let marker = TestCallback::new("marker", |_| Ok(json!(null)));
    let marker_calls = Arc::clone(&marker.calls);
    let sandbox = builder
        .with_callback(approve)
        .with_callback(marker)
        .build()
        .unwrap();

    let outcome = sandbox
        .execute_with_journal("approve || true; marker", None)
        .await;

    assert_eq!(marker_calls.load(Ordering::SeqCst), 0);
    let suspended = outcome.suspended.expect("execution should suspend");
    assert_eq!(suspended.name, "approve");
    assert!(matches!(
        outcome.result.unwrap_err().kind(),
        ErrorKind::Suspended(_)
    ));
}

/// A callback named like a builtin could only be reached through `invoke`,
/// and one named like a command would hide it, so neither builds (#962).
#[tokio::test]
async fn callbacks_named_like_builtins_or_commands_are_refused() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let err = builder
        .with_callback(TestCallback::new("echo", |_| Ok(json!(null))))
        .build()
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("callback 'echo' collides with the shell builtin or reserved word 'echo'"),
        "{err}"
    );
    // The embedded coreutils' `cat`.
    let err = shell_sandbox()
        .unwrap()
        .with_callback(TestCallback::new("cat", |_| Ok(json!(null))))
        .build()
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("callback 'cat' collides with the command 'cat'"),
        "{err}"
    );
    // Without the embedded commands, `cat` is free.
    shell_sandbox()
        .unwrap()
        .without_embedded_commands()
        .with_callback(TestCallback::new("cat", |_| Ok(json!(null))))
        .build()
        .unwrap();
}

#[tokio::test]
async fn grep_builtin_filters_lines() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(
            r#"
printf 'apple\nbanana\nCherry\n' | grep -n an
printf 'apple\nbanana\nCherry\n' | grep -ic cherry
printf 'a\n' | grep -q zzz; echo "none=$?"
printf 'a.c\nabc\n' | grep -F a.c
printf 'x\nab\nb\n' | grep -c 'a\|b'
printf 'a|b\nb\n' | grep -cE 'a|b'
printf 'good\nba\377d\nlast\n' | grep -c d
grep x /no/such/file; echo "missing=$?"
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "2:banana\n1\nnone=1\na.c\n2\n2\n2\nmissing=2\n"
    );
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("grep: /no/such/file: "));
}

/// Not an assertion target: numbers for the PR write-up. Run in release:
/// `cargo test --release -p eryx --features embedded,shell --test shell_guest
/// -- --ignored --nocapture latency`.
#[tokio::test]
#[ignore = "benchmark numbers, not a check"]
async fn latency_numbers() {
    const N: u32 = 50;
    let start = std::time::Instant::now();
    let sandbox = shell_sandbox().unwrap().build().unwrap();
    sandbox.execute("echo warm").await.unwrap();
    eprintln!("first sandbox + execute: {:?}", start.elapsed());

    let start = std::time::Instant::now();
    for _ in 0..N {
        sandbox.execute("echo hi").await.unwrap();
    }
    eprintln!("stateless `echo hi`: {:?}/exec", start.elapsed() / N);

    let mut session = SessionExecutor::new(shell_executor().unwrap(), &[])
        .await
        .unwrap();
    session.execute("x=0").run().await.unwrap();
    let start = std::time::Instant::now();
    for _ in 0..N {
        session.execute("x=$((x + 1))").run().await.unwrap();
    }
    eprintln!("session `x=$((x+1))`: {:?}/exec", start.elapsed() / N);
}

/// A shell snapshot names its state format, and a runtime refuses one in
/// another format with a distinct error rather than a parse failure.
#[tokio::test]
async fn snapshots_in_another_state_format_are_refused() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(Arc::clone(&executor), &[])
        .await
        .unwrap();
    session.execute("x=1").run().await.unwrap();
    let bytes = session.snapshot_state().await.unwrap().to_bytes();

    const FORMAT: &[u8] = b"eryx-shell-state/1 brush@";
    let at = bytes
        .windows(FORMAT.len())
        .position(|w| w == FORMAT)
        .expect("the snapshot names its state format")
        + FORMAT.len();
    let mut other = bytes[..at].to_vec();
    other.extend_from_slice(b"0000000");
    let rest = &bytes[at..];
    let newline = rest.iter().position(|&b| b == b'\n').unwrap();
    other.extend_from_slice(&rest[newline..]);
    let other = eryx::StateSnapshot::from_bytes(&other).unwrap();

    let mut fresh = SessionExecutor::new(executor, &[]).await.unwrap();
    let err = fresh.restore_state(&other).await.unwrap_err();
    let ErrorKind::IncompatibleStateFormat {
        snapshot, runtime, ..
    } = err.kind()
    else {
        panic!("expected IncompatibleStateFormat, got {err:?}");
    };
    assert_eq!(snapshot, "eryx-shell-state/1 brush@0000000");
    assert!(
        runtime.starts_with("eryx-shell-state/1 brush@"),
        "{runtime}"
    );
}

/// The shell's baseline environment and filesystem (#832): a statable `/`
/// with the mounts beneath it, a writable `/tmp` and the `/dev` devices.
/// Builtins only, since children don't see this filesystem without
/// `unstable-vfs` yet.
#[tokio::test]
async fn the_shell_has_a_baseline_unix_environment() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(r#"echo "$HOME|$PATH|$TERM|$LANG|$LC_ALL|$USER|$PWD|$TMPDIR""#)
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "/tmp|/eryx/bin|dumb|C.UTF-8|C.UTF-8|eryx|/|/tmp\n"
    );

    let out = sandbox
        .execute(
            r#"
[ -d / ] && [ -d /tmp ] && [ -d /dev ] && echo dirs
[ -e /dev/null ] && [ -e /dev/zero ] && [ -e /dev/urandom ] && echo devices
echo hi > /tmp/f && read -r line < /tmp/f && echo "tmp:$line"
echo gone > /dev/null && echo "null:$(< /dev/null):"
cd / && echo "cwd:$PWD"
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "dirs\ndevices\ntmp:hi\nnull::\ncwd:/\n"
    );
}

/// Nothing above the mounts can be created or written, and paths outside
/// every mount don't exist.
#[tokio::test]
async fn the_shell_root_is_read_only() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            r#"
if echo x > /new.txt; then echo created; else echo refused; fi
if echo x > /etc/passwd; then echo created; else echo refused; fi
[ -e /etc/passwd ] || echo missing
if read -r line < /etc/passwd; then echo read; else echo unreadable; fi
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "refused\nrefused\nmissing\nunreadable\n"
    );
}

/// The shell's `/tmp` counts against the VFS quota, even without a VFS of
/// the caller's.
#[tokio::test]
async fn the_shell_tmp_is_bounded_by_the_vfs_quota() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_resource_limits(eryx::ResourceLimits::default().with_max_vfs_bytes(64 * 1024))
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
if printf '%*s' 32768 x > /tmp/small; then echo small; fi
if printf '%*s' 200000 x > /tmp/big; then echo big; else echo full; fi
"#,
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "small\nfull\n");
}

/// The shell's `/tmp` is also bounded by the VFS entry limit.
#[tokio::test]
async fn the_shell_tmp_is_bounded_by_the_vfs_entry_limit() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_resource_limits(eryx::ResourceLimits::default().with_max_vfs_entries(16))
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
n=0; for i in {1..32}; do if : > /tmp/f$i; then n=$((n+1)); fi; done 2>/dev/null
echo $n
"#,
        )
        .await
        .unwrap();
    let created: usize = out.stdout_utf8().unwrap().trim().parse().unwrap();
    assert!((1..16).contains(&created), "{created}");
}

/// A session's `/tmp` outlives a reset, as the VFS does.
#[tokio::test]
async fn the_shell_tmp_survives_a_session_reset() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    session.execute("echo kept > /tmp/k").run().await.unwrap();
    session.reset(&[]).await.unwrap();
    let out = session
        .execute("read -r line < /tmp/k && echo \"$line\"")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "kept\n");
}

/// A single-file volume at the top level doesn't hide the root: `/` still
/// lists, `/dev` is still there, and the file is served.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn a_top_level_file_volume_keeps_the_root() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("settings.json"), "{}\n").unwrap();
    let sandbox = builder
        .with_volume(eryx::VolumeMount::read_only(
            host.path().join("settings.json"),
            "/cfg.json",
        ))
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
[ -d / ] && [ -e /dev/null ] && [ -d /tmp ] && echo root
read -r cfg < /cfg.json && echo "cfg:$cfg"
for f in /*; do printf '%s ' "$f"; done; echo
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "root\ncfg:{}\n/cfg.json /data /dev /tmp \n"
    );
}

/// Callback replay matches calls on their arguments' text, which the shell
/// writes with sorted keys: the same flags in another order replay. And a
/// run that suspends resumes from its journal.
#[tokio::test]
async fn replay_ignores_flag_order_and_resumes_after_suspending() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let tool = TestCallback::new("tool", Ok);
    let calls = Arc::clone(&tool.calls);
    let sandbox = builder.with_callback(tool).build().unwrap();
    let first = sandbox.execute_with_journal("tool --b 1 --a 2", None).await;
    first.result.unwrap();
    let second = sandbox
        .execute_with_journal("tool --a 2 --b 1", Some(first.journal))
        .await;
    assert_eq!(second.replayed_callbacks, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    second.result.unwrap();

    const SCRIPT: &str = "before --n 1; approve; after";
    let suspending = shell_sandbox()
        .unwrap()
        .with_callback(TestCallback::new("before", Ok))
        .with_callback(TestCallback::new("approve", |_| {
            Err(CallbackError::Suspend("needs approval".to_string()))
        }))
        .with_callback(TestCallback::new("after", Ok))
        .build()
        .unwrap();
    let halted = suspending.execute_with_journal(SCRIPT, None).await;
    assert_eq!(halted.suspended.expect("suspends").name, "approve");

    let before = TestCallback::new("before", Ok);
    let before_calls = Arc::clone(&before.calls);
    let after = TestCallback::new("after", Ok);
    let after_calls = Arc::clone(&after.calls);
    let approving = shell_sandbox()
        .unwrap()
        .with_callback(before)
        .with_callback(TestCallback::new("approve", |_| Ok(json!(true))))
        .with_callback(after)
        .build()
        .unwrap();
    let resumed = approving
        .execute_with_journal(SCRIPT, Some(halted.journal))
        .await;
    resumed.result.unwrap();
    assert_eq!(resumed.replayed_callbacks, 1);
    assert_eq!(before_calls.load(Ordering::SeqCst), 0, "replayed");
    assert_eq!(after_calls.load(Ordering::SeqCst), 1, "live");
}
