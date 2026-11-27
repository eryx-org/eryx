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

    // A missing command is a script failure too, with bash's status. The
    // message quotes the last line of stderr after the status, and the
    // stderr itself is in `stderr`.
    let err = sandbox
        .execute("echo first >&2; no-such-command")
        .await
        .unwrap_err();
    let ErrorKind::ScriptFailed {
        message,
        exit_code,
        stderr,
        ..
    } = err.kind()
    else {
        panic!("expected a script failure, got {err:?}");
    };
    assert_eq!(*exit_code, Some(127));
    let stderr = String::from_utf8_lossy(stderr);
    let last = stderr.lines().rfind(|line| !line.is_empty()).unwrap();
    assert!(last.contains("no-such-command"), "{stderr:?}");
    assert_eq!(*message, format!("exit status 127: {last}"));
    assert!(stderr.starts_with("first\n"), "{stderr:?}");
}

/// The message quotes stderr's last line, so it's scrubbed like stderr: a
/// secret the script prints (as its placeholder; the real value never
/// reaches the guest) is `[REDACTED]` in both.
#[tokio::test]
async fn a_failure_message_quoting_stderr_is_scrubbed() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_secret("TOKEN", "real-value", vec![])
        .build()
        .unwrap();

    let err = sandbox
        .execute("echo \"token=$TOKEN\" >&2; exit 1")
        .await
        .unwrap_err();
    let ErrorKind::ScriptFailed {
        message, stderr, ..
    } = err.kind()
    else {
        panic!("expected a script failure, got {err:?}");
    };
    let stderr = String::from_utf8_lossy(stderr);
    assert_eq!(message, "exit status 1: token=[REDACTED]", "{stderr:?}");
    assert_eq!(stderr, "token=[REDACTED]\n");
    assert!(!err.to_string().contains("real-value"), "{err}");
    assert!(!err.to_string().contains("ERYX_SECRET"), "{err}");

    // A placeholder across the message's 1024-character cap isn't split
    // into an unscrubbable fragment: it's dropped whole from the message
    // (and scrubbed in stderr, which has it all).
    let pad = "-".repeat(1000);
    let err = sandbox
        .execute(&format!("echo \"{pad} $TOKEN tail\" >&2; exit 1"))
        .await
        .unwrap_err();
    let ErrorKind::ScriptFailed {
        message, stderr, ..
    } = err.kind()
    else {
        panic!("expected a script failure, got {err:?}");
    };
    assert_eq!(*message, format!("exit status 1: {pad} …"));
    assert_eq!(
        String::from_utf8_lossy(stderr),
        format!("{pad} [REDACTED] tail\n")
    );
    assert!(!err.to_string().contains("ERYX_SECRET"), "{err}");
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

/// A shell sandbox where `python` and `python3` run the embedded Python
/// runtime as a command (#831).
fn python_shell() -> Option<SandboxBuilder<Shell, state::Has>> {
    let python = eryx::Command::guest(Language::Python).unwrap();
    Some(shell_sandbox()?.with_commands(["python", "python3"], python))
}

#[tokio::test]
async fn python3_is_opt_in() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let err = sandbox.execute("python3 -c 1").await.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ScriptFailed {
                exit_code: Some(127),
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn python3_streams_through_a_pipeline() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            "echo 'print(1 + 1)' | python3\n\
             seq 1 1000 | python3 -c 'import sys; print(len(sys.stdin.read()))' | head -1\n\
             python -c 'import sys; print(sys.argv)' a b",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "2\n3893\n['-c', 'a', 'b']\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Output past a pipe's 64 KiB arrives whole and in order, into a pipe or
/// a file, on stdout and stderr alike (#1239): a write takes only what the
/// pipe has room for, and unbuffered stdio dropped the rest.
#[tokio::test]
async fn python3_writes_large_output_whole() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    // 600000 numbered lines, about 3.9 MB: the same bytes as `seq`.
    let lines = "'\\n'.join(map(str, range(1, 600001))) + '\\n'";
    let out = sandbox
        .execute(&format!(
            "seq 1 600000 | md5sum\n\
             python3 -c \"import sys; sys.stdout.write({lines})\" | md5sum\n\
             python3 -c \"print({lines}, end='')\" > /tmp/o.txt; md5sum < /tmp/o.txt\n\
             python3 -c \"import sys; sys.stdout.buffer.write(({lines}).encode())\" | md5sum\n\
             python3 -c \"import sys; sys.stderr.write({lines}); print('out')\" 2>/tmp/e.txt\n\
             md5sum < /tmp/e.txt",
        ))
        .await
        .unwrap();
    let stdout = out.stdout_utf8().unwrap();
    let mut sums = stdout.lines().filter(|l| *l != "out");
    let seq = sums.next().unwrap();
    assert_eq!(sums.clone().count(), 4, "{stdout}");
    assert!(sums.all(|sum| sum == seq), "{stdout}");
    assert!(stdout.contains("out\n"), "{stdout}");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `timeout` stops a CPU-bound guest child: Python spinning never waits,
/// but the host still polls the shell, whose timer kills it.
///
/// 0.3 s keeps it under the host's per-child fuel cap (`CHILD_FUEL` in
/// src/process.rs): a busy Python loop exhausts it in about 0.8 s and the
/// child dies of that first (134), so `timeout 1` would never see it run out.
#[tokio::test]
async fn timeout_stops_a_busy_python3() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let start = std::time::Instant::now();
    let out = sandbox
        .execute("timeout 0.3 python3 -c 'while True: pass'; echo \"s=$?\"")
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "s=124\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn python3_shares_the_shells_files_env_and_cwd() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            "echo from-sh > /tmp/x\n\
             python3 -c \"print(open('/tmp/x').read().strip()); open('/tmp/y', 'w').write('from-py')\"\n\
             cat /tmp/y; echo\n\
             FOO=1 python3 -c 'import os; print(os.environ[\"FOO\"])'\n\
             cd /tmp && python3 -c 'import os; print(os.getcwd())'",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "from-sh\nfrom-py\n1\n/tmp\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn python3_exit_status_and_traceback_reach_the_shell() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            "python3 -c 'import sys; sys.exit(3)'; echo $?\n\
             python3 -c 'raise ValueError(\"boom\")' 2>/tmp/err; echo $?\n\
             tail -1 /tmp/err",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "3\n1\nValueError: boom\n");
}

#[tokio::test]
async fn python3_burns_the_executions_fuel() {
    let Some(builder) = python_shell() else {
        return;
    };
    let mut session = SessionExecutor::new(builder.build().unwrap().executor(), &[])
        .await
        .unwrap();
    let err = session
        .execute("python3 -c 'while True: pass'")
        .with_fuel_limit(500_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );
}

/// An execution's peak memory counts its children's, held at the same time
/// as the shell's own.
#[tokio::test]
async fn peak_memory_includes_child_processes() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let alone = sandbox.execute("true").await.unwrap();
    let alone = alone.stats.peak_memory_bytes.unwrap();
    let out = sandbox
        .execute("python3 -c 'b = bytearray(100 * 1024 * 1024)'")
        .await
        .unwrap();
    let peak = out.stats.peak_memory_bytes.unwrap();
    assert!(peak > alone + 100 * 1024 * 1024, "{peak} vs {alone} alone");
}

#[tokio::test]
async fn python3_memory_counts_against_the_child_pool() {
    let Some(builder) = python_shell() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    // More than the children's 256 MiB pool.
    let out = sandbox
        .execute(
            "python3 -c 'b = bytearray(300 * 1024 * 1024)' 2>/tmp/err; echo $?\n\
             tail -1 /tmp/err\n\
             echo 'import sys; sys.stdin.read(); print(\"ok\")' > /tmp/s.py\n\
             python3 /tmp/s.py < /tmp/s.py | python3 /tmp/s.py | python3 /tmp/s.py | python3 /tmp/s.py",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "1\nMemoryError\nok\n");
}

#[tokio::test]
async fn guest_commands_cant_have_credentials() {
    let Some(builder) = python_shell() else {
        return;
    };
    let result = builder
        .with_network(eryx::NetConfig::default())
        .with_child_network(eryx::ChildNetwork::Inherit)
        .with_credential(
            eryx::net::Credential::bearer("token")
                .for_hosts(["example.com"])
                .for_commands(["python3"]),
        )
        .build();
    let err = match result {
        Ok(sandbox) => sandbox.execute("true").await.unwrap_err(),
        Err(e) => e,
    };
    assert!(err.to_string().contains("guest runtime"), "{err}");
}

/// A shell sandbox where `node` and `js` run the embedded JavaScript
/// runtime as a command (#831).
#[cfg(feature = "javascript")]
fn node_shell() -> SandboxBuilder<Shell, state::Has> {
    let node = eryx::Command::guest(Language::JavaScript).unwrap();
    shell_sandbox().unwrap().with_commands(["node", "js"], node)
}

#[cfg(feature = "javascript")]
async fn run_node_shell(code: &str) -> (String, String) {
    let out = node_shell().build().unwrap().execute(code).await.unwrap();
    (
        out.stdout_utf8().unwrap().to_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_is_opt_in() {
    let sandbox = shell_sandbox().unwrap().build().unwrap();
    let err = sandbox.execute("node -e 1").await.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            ErrorKind::ScriptFailed {
                exit_code: Some(127),
                ..
            }
        ),
        "{err:?}"
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_runs_code_and_pipelines() {
    let (stdout, stderr) = run_node_shell(
        "node -e 'console.log(1+1)'\n\
         echo 'console.log(2)' | node\n\
         node -p '6*7'\n\
         js -e 'console.log(process.argv.slice(1).join(\" \"))' a b\n\
         node -e 'for (let i = 0; i < 5; i++) console.log(i)' | wc -l\n\
         seq 1 1000 | node -e 'console.log(require(\"fs\").readFileSync(0, \"utf8\").length)'\n\
         node -e 'for (let i = 0; i < 100000; i++) console.log(i)' | head -2",
    )
    .await;
    assert_eq!(stdout, "2\n2\n42\na b\n5\n3893\n0\n1\n", "{stderr}");
}

/// Output past a pipe's 64 KiB arrives whole and in order (#1239).
#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_writes_large_output_whole() {
    let (stdout, stderr) = run_node_shell(
        "seq 1 600000 | md5sum\n\
         node -e 'let s = \"\"; for (let i = 1; i <= 600000; i++) s += i + \"\\n\"; process.stdout.write(s)' | md5sum\n\
         node -e 'let s = \"\"; for (let i = 1; i <= 600000; i++) s += i + \"\\n\"; process.stdout.write(s)' > /tmp/o.txt\n\
         md5sum < /tmp/o.txt",
    )
    .await;
    let sums: Vec<&str> = stdout.lines().collect();
    assert_eq!(sums.len(), 3, "{stdout} {stderr}");
    assert!(sums.iter().all(|sum| *sum == sums[0]), "{stdout} {stderr}");
}

/// A command that keeps printing ends when its reader goes, rather than
/// running until the execution's timeout.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_stops_when_its_reader_goes() {
    let sandbox = node_shell()
        .with_resource_limits(
            eryx::ResourceLimits::default()
                .with_execution_timeout(std::time::Duration::from_secs(10)),
        )
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    let out = sandbox
        .execute(
            "node -e 'setInterval(() => console.log(1), 0)' | head -1\n\
             node -e 'for (;;) console.log(2)' | head -1\n\
             node -e 'for (;;) process.stdout.write(\"3\\n\")' | head -1",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "1\n2\n3\n");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_runs_scripts_with_shebangs_and_args() {
    let (stdout, stderr) = run_node_shell(
        r#"cd /tmp
printf '%s\n' '#!/usr/bin/env node' 'console.log(process.argv.slice(2), __filename)' > s.js
node s.js a b
./s.js c
printf 'module.exports = 6\n' > six.js
node -e 'console.log(require("./six") * 7)'
printf '%s\n' '#!/usr/bin/env node' '"use strict"' 'import path from "node:path"' 'console.log(path.join("d", "e"))' > i.js
./i.js
"#,
    )
    .await;
    assert_eq!(
        stdout, "[\"a\",\"b\"] /tmp/s.js\n[\"c\"] /tmp/s.js\n42\nd/e\n",
        "{stderr}"
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_exit_status_and_errors_reach_the_shell() {
    let (stdout, stderr) = run_node_shell(
        "node -e 'process.exit(3)'; echo $?\n\
         node -e 'throw new Error(\"boom\")' 2>/tmp/err; echo $?\n\
         head -1 /tmp/err\n\
         node --bogus 2>/dev/null; echo $?",
    )
    .await;
    assert_eq!(stdout, "3\n1\nError: boom\n9\n", "{stderr}");
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_shares_the_shells_files_env_and_cwd() {
    let (stdout, stderr) = run_node_shell(
        "echo from-sh > /tmp/x\n\
         node -e \"const fs = require('fs'); console.log(fs.readFileSync('/tmp/x', 'utf8').trim()); fs.writeFileSync('/tmp/y', 'from-node')\"\n\
         cat /tmp/y; echo\n\
         FOO=1 node -e 'console.log(process.env.FOO)'\n\
         cd /tmp && node -e 'console.log(process.cwd(), require(\"fs\").existsSync(\"x\"))'",
    )
    .await;
    assert_eq!(stdout, "from-sh\nfrom-node\n1\n/tmp true\n", "{stderr}");
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_burns_the_executions_fuel_and_time() {
    let mut session = SessionExecutor::new(node_shell().build().unwrap().executor(), &[])
        .await
        .unwrap();
    let err = session
        .execute("node -e 'while (true) {}'")
        .with_fuel_limit(500_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );

    let sandbox = node_shell()
        .with_resource_limits(
            eryx::ResourceLimits::default()
                .with_execution_timeout(std::time::Duration::from_millis(100)),
        )
        .build()
        .unwrap();
    let err = sandbox
        .execute("node -e 'while (true) {}'")
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Timeout(_)),
        "expected Timeout, got {err:?}"
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn node_memory_counts_against_the_child_pool() {
    let (stdout, stderr) = run_node_shell(
        // More than the children's 256 MiB pool.
        "node -e 'new Uint8Array(300 * 1024 * 1024).fill(1)' 2>/dev/null; echo $?\n\
         node -e 'console.log(new Uint8Array(1 << 20).length)'",
    )
    .await;
    assert_eq!(stdout, "1\n1048576\n", "{stderr}");
}

/// Run `code` in a fresh shell sandbox, and return its stdout and stderr.
async fn run_shell(code: &str) -> (String, String) {
    let sandbox = shell_sandbox().unwrap().build().unwrap();
    let out = sandbox.execute(code).await.unwrap();
    (
        out.stdout_utf8().unwrap().to_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `sh` and `bash` run a nested shell, like an external command: it sees
/// only exported variables, nothing it changes comes back, and its exit
/// status is the command's.
#[tokio::test]
async fn sh_and_bash_run_nested_shells() {
    let (stdout, stderr) = run_shell(
        r#"
bash -c 'echo $((1+2))'
sh -c 'exit 7'; echo "status=$?"
X=1; export Y=2; bash -c 'echo "[$X$Y]"'
bash -c 'echo $0 $1 $2' me a b
echo 'echo from stdin' | bash
echo 'echo "$1"' | bash -s arg
cd /tmp; bash -c 'cd /; Z=1'; echo "$PWD [$Z]"
bash -ec 'false; echo not reached'; echo "e=$?"
bash -o pipefail -c 'false | true'; echo "pipefail=$?"
bash -c 'echo nested >&2'
bash -c 'bash -c "sh -c \"echo deep\""'
bash --login; echo "bad=$?"
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "3\nstatus=7\n[2]\nme a b\nfrom stdin\narg\n/tmp []\ne=1\npipefail=1\ndeep\nbad=2\n"
    );
    assert!(
        stderr.starts_with("nested\nbash: --login: invalid option\n"),
        "{stderr}"
    );
}

/// `sh FILE` and `bash FILE` run a script with its arguments.
#[tokio::test]
async fn sh_runs_a_script_file() {
    let (stdout, stderr) = run_shell(
        r#"
printf 'echo "$0 $# $1 $2"\nexit 4\n' > /tmp/f.sh
sh /tmp/f.sh a b; echo "status=$?"
cd /tmp && bash f.sh x; echo "status=$?"
bash /tmp/missing.sh; echo "missing=$?"
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "/tmp/f.sh 2 a b\nstatus=4\nf.sh 1 x \nstatus=4\nmissing=127\n"
    );
    assert!(stderr.contains("missing.sh"), "{stderr}");
}

/// A nested shell is a pipeline stage like any other: its stdin and stdout
/// are the pipes, a writer that wouldn't end for minutes stops once a
/// nested `head` is done (and the other way round), and nested children's
/// output reaches the outer pipe.
#[tokio::test]
async fn nested_shells_stream_through_pipelines() {
    let (stdout, _) = run_shell(
        r#"
seq 1 1000000000 | bash -c 'head -n1'
bash -c 'seq 1 1000000000' | head -n1
seq 1 1000000000 | bash -c 'read x; echo "read $x"'
echo hi | bash -c cat | cat
seq 1 100000 | bash -c 'wc -l'
bash -c 'seq 1 1000000000 | sh -c "head -n2"' | tail -n1
"#,
    )
    .await;
    assert_eq!(stdout, "1\n1\nread 1\nhi\n100000\n2\n");
}

/// Callbacks are commands in a nested shell too.
#[tokio::test]
async fn callbacks_reach_nested_shells() {
    let sandbox = shell_sandbox()
        .unwrap()
        .with_callback(TestCallback::new("add", add).with_schema(add_schema()))
        .build()
        .unwrap();
    let out = sandbox
        .execute("bash -c 'add 2 3'; sh -c 'invoke add \"{\\\"a\\\": 1, \\\"b\\\": 1}\"'")
        .await
        .unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "5\n2\n");
}

/// Shells nested past the cap exit 126 with a message, rather than recurse
/// until the guest runs out of memory.
#[tokio::test]
async fn nesting_shells_too_deep_fails_cleanly() {
    let (stdout, stderr) = run_shell(
        r#"
printf 'bash /tmp/r.sh\n' > /tmp/r.sh
bash /tmp/r.sh; echo "status=$?"
"#,
    )
    .await;
    assert_eq!(stdout, "status=126\n");
    assert!(
        stderr.contains("bash: shells nested more than 16 deep"),
        "{stderr}"
    );
}

/// Any file is runnable: its `#!` line names the interpreter, by its file
/// name (a shell runs nested), and a file without one is an `sh` script.
/// A path is never a host command by its file name: `./ls` is a file.
#[tokio::test]
async fn files_run_as_scripts() {
    let (stdout, stderr) = run_shell(
        r#"
cd /tmp
printf '#!/bin/sh\necho "sh $0 $1"\nexit 3\n' > a.sh
./a.sh x; echo "status=$?"
printf '#!/usr/bin/env bash\necho "bash $#"\n' > b
/tmp/b 1 2
printf 'echo "plain $1"\n' > c
./c y
printf '#!/bin/bash -e\nfalse\necho not reached\n' > d
./d; echo "e=$?"
printf 'echo on-path\n' > tool
PATH="/tmp:$PATH" tool
./ls; echo "ls=$?"
printf '#!/usr/bin/env nope\n' > e
./e; echo "nope=$?"
./; echo "dir=$?"
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "sh ./a.sh x\nstatus=3\nbash 2\nplain y\ne=1\non-path\nls=127\nnope=127\ndir=126\n"
    );
    assert!(stderr.contains("nope: command not found"), "{stderr}");
}

/// A `#!` naming another interpreter runs the host command of that name,
/// with the script's path as an argument.
#[tokio::test]
async fn shebangs_run_host_commands() {
    let (stdout, _) = run_shell(
        r#"
printf '#!/usr/bin/cat\nhello\n' > /tmp/s
/tmp/s
printf '#!/usr/bin/env head -n1\nfirst\nsecond\n' > /tmp/h
/tmp/h
"#,
    )
    .await;
    assert_eq!(stdout, "#!/usr/bin/cat\nhello\n#!/usr/bin/env head -n1\n");
}

/// `chmod` changes nothing (WASI has no permission bits, and any file runs),
/// but accepts the usual modes so `chmod +x x && ./x` works under `set -e`,
/// and fails like GNU's on a missing file or a bad mode (#1092).
#[tokio::test]
async fn chmod_accepts_modes_and_checks_files() {
    let (stdout, stderr) = run_shell(
        r#"
set -e
cd /tmp
printf 'echo ran\n' > s.sh; touch t; mkdir -p d/e
chmod +x s.sh && ./s.sh
chmod u+x,go-w s.sh
chmod 755 s.sh t; chmod -R a+rX d; chmod -x t; chmod -- 0644 t
before=$(ls -l s.sh); chmod 600 s.sh; [ "$before" = "$(ls -l s.sh)" ] && echo same
set +e
chmod +x nope; echo "missing=$?"
chmod -f +x nope; echo "quiet=$?"
chmod zz t; echo "mode=$?"
chmod +x; echo "operand=$?"
"#,
    )
    .await;
    assert_eq!(stdout, "ran\nsame\nmissing=1\nquiet=1\nmode=1\noperand=1\n");
    assert_eq!(
        stderr,
        "chmod: cannot access 'nope': No such file or directory\n\
         chmod: invalid mode: 'zz'\nTry 'chmod --help' for more information.\n\
         chmod: missing operand after '+x'\nTry 'chmod --help' for more information.\n"
    );
}

/// `env` is a builtin (uutils' can't run commands on WASI): it prints the
/// exported environment, or runs a command, found as any other is, with
/// variables added (`NAME=VALUE`), removed (`-u`) or only those (`-i`).
#[tokio::test]
async fn env_runs_commands_in_a_changed_environment() {
    let (stdout, stderr) = run_shell(
        r#"
export A=1; B=2
env | grep -E '^(A|B)='
env C=3 D=4 bash -c 'echo "$A $C $D"'
env -i E=5 sh -c 'echo "[$A] $E"'
env -i | wc -l
env -u A sh -c 'echo "[$A]"'
printf 'echo "$A"\n' > /tmp/s; env A=x /tmp/s
echo "A=$A"
env sh -c 'exit 9'; echo "status=$?"
env nope; echo "missing=$?"
env -S 'a b'; echo "bad=$?"
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "A=1\n1 3 4\n[] 5\n0\n[]\nx\nA=1\nstatus=9\nmissing=127\nbad=125\n"
    );
    assert!(stderr.contains("env: invalid option"), "{stderr}");
}

/// `timeout` is a builtin (uutils' needs processes and signals): it runs its
/// command, found as `env`'s is, and passes its status through; GNU's
/// statuses for its own failures and a command that can't be found.
#[tokio::test]
async fn timeout_runs_commands_and_passes_their_status_through() {
    let (stdout, stderr) = run_shell(
        r#"
type timeout
timeout 5 echo hi; echo "s=$?"
timeout 5 sh -c 'exit 3'; echo "s=$?"
timeout 0 sh -c 'echo none'; echo "s=$?"
timeout 1m grep -c x /dev/null; echo "s=$?"
timeout 1 nosuchcmd; echo "missing=$?"
timeout x echo; echo "bad=$?"
timeout --help | head -n 1
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "timeout is a shell builtin\nhi\ns=0\ns=3\nnone\ns=0\n0\ns=1\nmissing=127\nbad=125\n\
         Usage: timeout [OPTION] DURATION COMMAND [ARG]...\n"
    );
    assert!(
        stderr.contains("timeout: invalid time interval 'x'"),
        "{stderr}"
    );
}

/// A command still running when the duration is up is stopped at once:
/// `timeout` exits 124, or 128 plus the signal under `--preserve-status`.
#[tokio::test]
async fn timeout_stops_a_command_that_runs_too_long() {
    let start = std::time::Instant::now();
    let (stdout, stderr) = run_shell(
        r#"
timeout 0.2 sleep 5; echo "s=$?"
timeout --preserve-status -s KILL 0.2 sh -c 'sleep 5; echo late'; echo "s=$?"
"#,
    )
    .await;
    assert_eq!(stdout, "s=124\ns=137\n");
    assert_eq!(stderr, "");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

/// A background job made only of builtins (no child processes) runs, and
/// `kill` stops one that would loop forever, with its pipeline stages.
#[tokio::test]
async fn background_builtin_pipelines_run_and_can_be_killed() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            "{ i=0; while [ $i -lt 200 ]; do echo $i; i=$((i + 1)); done | { n=0; while read -r _; do n=$((n + 1)); done; echo \"counted $n\"; }; } &\n\
             wait $!; echo \"status: $?\"\n\
             { while :; do echo y; done | while read -r _; do :; done; } &\n\
             kill %1; wait %1; echo \"killed: $?\"",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "counted 200\nstatus: 0\nkilled: 143\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

/// A session's next execution starts with no jobs: the last one waited for
/// its own.
#[tokio::test]
async fn session_jobs_end_with_their_execution() {
    let Some(executor) = shell_executor() else {
        return;
    };
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    let out = session
        .execute("(sleep 0.1; echo job) & echo started")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"started\njob\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let out = session
        .execute("jobs; wait; echo \"$?\"; wait %1; echo \"$?\"")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"0\n127\n");
}

/// `test` tells file types apart: `/dev/null` is a character device, and `-f`
/// and `-d` still see files and directories.
#[tokio::test]
async fn test_tells_file_types_apart() {
    let Some(builder) = shell_sandbox() else {
        return;
    };
    let sandbox = builder.build().unwrap();
    let out = sandbox
        .execute(
            "[ -c /dev/null ] && echo yes\n\
             echo x > /tmp/f; mkdir -p /tmp/d\n\
             for t in -c -b -p -S -f -d; do\n\
               for p in /dev/null /tmp/f /tmp/d; do [ $t $p ] && echo \"$t $p\"; done\n\
             done; true",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "yes\n-c /dev/null\n-f /tmp/f\n-d /tmp/d\n"
    );
}

/// `xargs` is a builtin (uutils' can't run commands on WASI): it runs its
/// command, found as any other is (builtins such as `grep`, commands,
/// `sh`/`bash`), on items read from stdin, with GNU's options and quoting.
#[tokio::test]
async fn xargs_runs_commands_on_its_input() {
    let (stdout, stderr) = run_shell(
        r#"
type xargs
printf 'a\nb\n' | xargs echo
printf "x 'y z' \"w v\" u\\\\ t\n" | xargs -n 1 echo
printf 'a b\0c\0' | xargs -0 echo
printf 'a b\0c\0' | xargs -0 -n 1 echo
printf 'p\nq\n' | xargs -I {} sh -c 'echo "== {}"'
printf '' | xargs -r echo ran; echo "r=$?"
printf '' | xargs echo empty
printf 'hello\n' > /tmp/f1; printf 'bye\n' > /tmp/f2
printf '/tmp/f1\n/tmp/f2\n' | xargs grep -l hello
printf '1 2\n3\n' | xargs -t -L 1 echo
printf 'a,b,' | xargs -d , echo
printf 'k\nl\n' > /tmp/items; xargs -a /tmp/items echo
printf 'a b STOP c\n' | xargs -E STOP echo
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "xargs is a shell builtin\na b\nx\ny z\nw v\nu t\na b c\na b\nc\n== p\n== q\nr=0\nempty\n\
         /tmp/f1\n1 2\n3\na b\nk l\na b\n"
    );
    assert_eq!(stderr, "echo 1 2\necho 3\n");
}

/// `xargs` exits as GNU's does: 123 if a command failed, 124 if one exited
/// 255, 125 if one was killed, 126 or 127 if it can't be run or isn't
/// there, and 1 for its own errors.
#[tokio::test]
async fn xargs_exit_statuses_follow_gnu() {
    let (stdout, stderr) = run_shell(
        r#"
printf 'a\nb\n' | xargs -n 1 sh -c 'exit 0'; echo "ok=$?"
printf 'a\nb\n' | xargs -n 1 sh -c 'echo "$0"; [ "$0" = b ] && exit 3; exit 0'; echo "failed=$?"
printf 'a\nb\n' | xargs -n 1 sh -c 'echo "$0"; exit 255'; echo "255=$?"
echo a | xargs sh -c 'sleep 5 & kill $!; wait $!'; echo "killed=$?"
echo a | xargs /tmp; echo "dir=$?"
echo a | xargs nope; echo "missing=$?"
echo a | xargs -q; echo "bad=$?"
echo "'a" | xargs echo; echo "quote=$?"
"#,
    )
    .await;
    assert_eq!(
        stdout,
        "ok=0\na\nb\nfailed=123\na\n255=124\nkilled=125\ndir=126\nmissing=127\nbad=1\nquote=1\n"
    );
    for message in [
        "xargs: sh: exited with status 255; aborting",
        "xargs: sh: terminated by signal 15",
        "xargs: invalid option -- 'q'",
        "xargs: unmatched single quote",
    ] {
        assert!(stderr.contains(message), "{message}: {stderr}");
    }
}

/// `xargs -P` runs its commands at once: four one-second sleeps take about
/// a second, not four, and every run's output arrives. Killing an `xargs`
/// job stops the commands it's running.
#[tokio::test]
async fn xargs_runs_commands_in_parallel() {
    let sandbox = shell_sandbox().unwrap().build().unwrap();
    let parallel = |code: &'static str| {
        let sandbox = &sandbox;
        async move {
            let start = std::time::Instant::now();
            let out = sandbox.execute(code).await.unwrap();
            let elapsed = start.elapsed();
            assert!(
                elapsed < std::time::Duration::from_millis(2500),
                "{code}: {elapsed:?}"
            );
            out.stdout_utf8().unwrap().to_owned()
        }
    };
    let out =
        parallel("printf '1\\n2\\n3\\n4\\n' | xargs -P 4 -n 1 sh -c 'sleep 1; echo \"$0\"'").await;
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["1", "2", "3", "4"]);
    // Host commands too, and -P 0 is "as many as allowed".
    let out = parallel("printf '1 1 1 1' | xargs -P 0 -n 1 sleep; echo \"$?\"").await;
    assert_eq!(out, "0\n");
    let out = parallel(
        "seq 4 | xargs -P 2 -n 1 sh -c 'sleep 5; echo late' & sleep 0.2; kill %1; wait %1; echo \"killed: $?\"",
    )
    .await;
    assert_eq!(out, "killed: 143\n");
}

/// Long input is split into several command lines: at `-s`, and at the
/// host's limit on a command's arguments, so a host command never fails to
/// start for it.
#[tokio::test]
async fn xargs_splits_long_input_at_the_size_limits() {
    let (stdout, stderr) = run_shell(
        r#"
seq 3000 | xargs -s 2000 echo | wc -l
seq 3000 | xargs -s 2000 echo | wc -w
seq 100000 | xargs -s 100000000 sh -c 'echo $#' x | wc -l
seq 100000 | xargs -s 100000000 basename -a | wc -l
seq 10 | xargs -s 4 echo; echo "too_long=$?"
"#,
    )
    .await;
    // 2000 bytes of "echo\0" plus numbers of up to 4 digits and a NUL.
    let per_line = 1
        + (1..=3000)
            .scan(5usize, |used, n: usize| {
                let len = n.to_string().len() + 1;
                if *used + len > 2000 {
                    *used = 5 + len;
                    Some(1)
                } else {
                    *used += len;
                    Some(0)
                }
            })
            .sum::<usize>();
    assert_eq!(
        stdout,
        format!("{per_line}\n3000\n2\n100000\ntoo_long=1\n"),
        "{stderr}"
    );
    assert_eq!(stderr, "xargs: argument line too long\n");
}
