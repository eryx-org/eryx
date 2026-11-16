//! End-to-end tests for the QuickJS guest (`eryx-guest-js`), run through
//! the same host as the CPython guest from the embedded, precompiled
//! `runtime-javascript.cwasm`:
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_guest
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use eryx::language::JavaScript;
use eryx::{
    Callback, CallbackError, ErrorKind, Language, ResourceLimits, Sandbox, SandboxBuilder, Schema,
    SessionExecutor, state,
};
use serde_json::{Value, json};

/// A JavaScript sandbox on the embedded runtime: no wasm file, no stdlib.
fn js_sandbox() -> Option<SandboxBuilder<JavaScript, state::Has>> {
    Some(Sandbox::embedded().javascript())
}

async fn js_session() -> Option<SessionExecutor> {
    let executor = js_sandbox()?.build().unwrap().executor();
    assert_eq!(executor.language(), Language::JavaScript);
    Some(SessionExecutor::new(executor, &[]).await.unwrap())
}

/// A runtime is stamped with its language: a Python sandbox refuses the
/// JavaScript one, as a JavaScript sandbox does the Python one.
#[test]
fn a_runtime_only_builds_for_its_language() {
    let js = eryx::embedded::runtime_path(Language::JavaScript)
        .unwrap()
        .unwrap();
    // SAFETY: the embedded runtime, extracted by eryx itself.
    #[allow(unsafe_code)]
    let err = unsafe { Sandbox::builder().with_precompiled_file(js) }
        .build()
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("this precompiled runtime runs javascript, but the sandbox is for python"),
        "{err}"
    );

    // The Python runtime is a preinit bundle, whose layout eryx reads with
    // the `preinit` feature.
    #[cfg(feature = "preinit")]
    {
        let python = eryx::embedded::runtime_path(Language::Python)
            .unwrap()
            .unwrap();
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let err = unsafe {
            Sandbox::builder()
                .javascript()
                .with_precompiled_file(python)
        }
        .build()
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("runs python, but the sandbox is for javascript"),
            "{err}"
        );
    }
}

/// A test callback backed by a closure, optionally delayed and counted.
struct TestCallback {
    name: &'static str,
    delay_ms: u64,
    calls: Arc<AtomicU32>,
    handler: fn(Value) -> Result<Value, CallbackError>,
}

impl TestCallback {
    fn new(name: &'static str, handler: fn(Value) -> Result<Value, CallbackError>) -> Self {
        Self {
            name,
            delay_ms: 0,
            calls: Arc::new(AtomicU32::new(0)),
            handler,
        }
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
        Schema::empty()
    }
    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let delay = Duration::from_millis(self.delay_ms);
        let handler = self.handler;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            handler(args)
        })
    }
}

fn add(args: Value) -> Result<Value, CallbackError> {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(json!({ "sum": a + b }))
}

#[tokio::test]
async fn console_output_and_result_variable() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder.build().unwrap();

    let out = sandbox
        .execute(
            r#"
console.log("hi", 1, {a: 1}, [true, null]);
console.error("oops");
let result = {sum: 1 + 2};
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "hi 1 {\"a\":1} [true,null]\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    assert_eq!(out.result.as_deref(), Some("{\"sum\":3}"));
}

#[tokio::test]
async fn await_callback_by_name_and_via_invoke() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
const x = await add({a: 2, b: 3});
const y = await invoke("add", {a: x.sum, b: 10});
console.log(x.sum, y.sum, JSON.stringify(listCallbacks().map(cb => cb.name)));
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "5 15 [\"add\"]\n");
}

#[tokio::test]
async fn parallel_callbacks_complete_out_of_order() {
    let Some(builder) = js_sandbox() else { return };
    let mut slow = TestCallback::new("slow", |_| Ok(json!("slow")));
    slow.delay_ms = 100;
    let mut fast = TestCallback::new("fast", |_| Ok(json!("fast")));
    fast.delay_ms = 10;
    let sandbox = builder
        .with_callback(slow)
        .with_callback(fast)
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
const order = [];
const track = (p) => p.then((v) => { order.push(v); return v; });
const all = await Promise.all([track(slow()), track(fast()), track(slow()), track(fast())]);
console.log(all.join(","), order.join(","));
"#,
        )
        .await
        .unwrap();

    assert_eq!(
        out.stdout_utf8().unwrap(),
        "slow,fast,slow,fast fast,fast,slow,slow\n"
    );
}

#[tokio::test]
async fn callback_error_rejects_the_promise() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
try {
    await fail();
    console.log("unreachable");
} catch (e) {
    console.log("caught:", e instanceof Error, e.message.includes("nope"));
}
"#,
        )
        .await
        .unwrap();

    assert_eq!(out.stdout_utf8().unwrap(), "caught: true true\n");
}

#[tokio::test]
async fn uncaught_errors_are_script_failures() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .build()
        .unwrap();

    for (code, expected) in [
        // Thrown synchronously, before any await.
        ("throw new TypeError('boom')", "TypeError: boom"),
        // Rejected after the script suspended on a callback.
        (
            "await add({a: 1, b: 2}); throw new RangeError('late')",
            "RangeError: late",
        ),
        // Syntax errors are thrown by the compiler, not the promise.
        ("let = = 1", "SyntaxError"),
        // Throwing a non-Error value.
        ("throw 42", "Uncaught 42"),
    ] {
        let err = sandbox.execute(code).await.unwrap_err();
        let ErrorKind::ScriptFailed {
            message: msg,
            exit_code: None,
            ..
        } = err.kind()
        else {
            panic!("{code}: expected a script failure, got {err:?}");
        };
        assert!(msg.contains(expected), "{code}: {msg}");
    }
}

#[tokio::test]
async fn awaiting_a_promise_that_never_settles_is_an_error() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder.build().unwrap();

    let err = sandbox
        .execute("await new Promise(() => {})")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("never completed"), "{err}");
}

#[tokio::test]
async fn session_state_persists_across_executes() {
    let Some(mut session) = js_session().await else {
        return;
    };

    session
        .execute("var a = 1; let b = 2; const c = 3; function f() { return a + b + c; }")
        .run()
        .await
        .unwrap();
    let out = session
        .execute("b += 10; console.log(f())")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"16\n");

    // The result variable is consumed per execution.
    let out = session.execute("var result = 'first'").run().await.unwrap();
    assert_eq!(out.result.as_deref(), Some("\"first\""));
    let out = session.execute("1 + 1").run().await.unwrap();
    assert_eq!(out.result, None);
}

/// Positional arguments bind to the callback's schema parameters, as in
/// Python; a single plain object is keyword arguments.
#[tokio::test]
async fn positional_callback_arguments_bind_to_the_schema() {
    let Some(builder) = js_sandbox() else { return };
    let add = eryx::DynamicCallback::builder("add", "Add two numbers", |args| {
        Box::pin(async move {
            Ok(json!(
                args["a"].as_i64().unwrap() * 10 + args["b"].as_i64().unwrap()
            ))
        })
    })
    .param("a", "integer", "first", true)
    .param("b", "integer", "second", true)
    .build();
    // The first parameter (alphabetically) takes an object, so a lone object
    // is keyword arguments only if its keys are all parameter names.
    let configure = eryx::DynamicCallback::builder("configure", "Configure", |args| {
        Box::pin(async move { Ok(args) })
    })
    .param("options", "object", "options", false)
    .param("verbose", "boolean", "verbose", false)
    .build();
    let pair = eryx::DynamicCallback::builder("pair", "Echo a and b", |args| {
        Box::pin(async move { Ok(args) })
    })
    .param("a", "integer", "first", false)
    .param("b", "integer", "second", false)
    .build();
    let sandbox = builder
        .with_callback(add)
        .with_callback(configure)
        .with_callback(pair)
        .build()
        .unwrap();

    let out = sandbox
        .execute(
            r#"
console.log(await add(1, 2), await add({a: 3, b: 4}), await invoke("add", 5, 6));
console.log(JSON.stringify(await configure({retries: 3})));
console.log(JSON.stringify(await configure({options: {retries: 3}, verbose: true})));
console.log(JSON.stringify(await configure({})));
// Trailing undefineds are dropped; one before another argument is null.
for (const call of [
    () => invoke("pair"),
    () => invoke("pair", undefined),
    () => pair(1, undefined),
    () => pair(undefined, 2),
]) {
    console.log(JSON.stringify(await call()));
}
for (const call of [() => add(1, 2, 3), () => invoke("add", {__eryx_args__: [1]})]) {
    try { await call(); console.log("UNEXPECTED"); } catch (e) { console.log(String(e.message ?? e)); }
}
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "12 34 56\n\
         {\"options\":{\"retries\":3}}\n\
         {\"options\":{\"retries\":3},\"verbose\":true}\n\
         {}\n\
         {}\n\
         {}\n\
         {\"a\":1}\n\
         {\"a\":null,\"b\":2}\n\
         invalid arguments: add() takes 2 positional arguments but 3 were given\n\
         add() got the keyword argument '__eryx_args__', which is reserved for positional arguments\n"
    );
}

/// Top-level `let`/`const` bindings live for the session, so declaring one
/// again in a later execute throws, as in Node's REPL. `var`, function
/// declarations and plain assignment can be repeated.
#[tokio::test]
async fn redeclaring_a_let_across_executes_throws() {
    let Some(mut session) = js_session().await else {
        return;
    };

    session
        .execute("let x = 1; const y = 2; var z = 3")
        .run()
        .await
        .unwrap();
    for code in ["let x = 2", "const y = 3", "let y = 3"] {
        let err = session.execute(code).run().await.unwrap_err();
        let ErrorKind::ScriptFailed { message, .. } = err.kind() else {
            panic!("{code}: expected a script failure, got {err:?}");
        };
        assert!(message.contains("SyntaxError"), "{code}: {message}");
    }

    // The failed executes changed nothing, and these are fine.
    let out = session
        .execute("var z = 4; function f() { return 1 } x = 5; console.log(x, y, z)")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"5 2 4\n");
}

#[tokio::test]
async fn fuel_limit_stops_an_infinite_loop() {
    let Some(mut session) = js_session().await else {
        return;
    };

    let err = session
        .execute("while (true) {}")
        .with_fuel_limit(50_000_000)
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::FuelExhausted { .. }),
        "expected FuelExhausted, got {err:?}"
    );
    // The trapped instance is discarded: the session starts again.
    let out = session.execute("console.log('next')").run().await.unwrap();
    assert_eq!(out.stdout_text(), "next\n");
    assert!(out.state_was_reset);
}

#[tokio::test]
async fn suspending_callback_halts_the_guest() {
    let Some(builder) = js_sandbox() else { return };
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
        .execute_with_journal("try { await approve() } catch {} await marker()", None)
        .await;

    assert_eq!(marker_calls.load(Ordering::SeqCst), 0);
    let suspended = outcome.suspended.expect("execution should suspend");
    assert_eq!(suspended.name, "approve");
    assert!(matches!(
        outcome.result.unwrap_err().kind(),
        ErrorKind::Suspended(_)
    ));
}

#[tokio::test]
async fn trivial_execute_fuel_cost() {
    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_resource_limits(ResourceLimits::default())
        .build()
        .unwrap();

    let out = sandbox.execute("1").await.unwrap();
    // Not an assertion target: the number, for comparing runs.
    eprintln!("JS trivial execute fuel: {:?}", out.stats.fuel_consumed);
    assert!(out.stats.fuel_consumed.is_some());
}

/// Run `code` and return its stdout as text.
async fn stdout_of(session: &mut SessionExecutor, code: &str) -> String {
    let out = session.execute(code).run().await.unwrap();
    String::from_utf8(out.stdout).unwrap()
}

/// A snapshot keeps the session's top-level variables, `var` and `let`/
/// `const` alike, with shared and cyclic objects intact, and names the
/// ones it can't serialize.
#[tokio::test]
async fn snapshot_round_trips_session_state() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session
        .execute(
            r#"
var count = 41;
let shared = { n: 1n << 70n, when: new Date(0), bytes: new Uint8Array([1, 2]) };
const pair = [shared, shared];
globalThis.cycle = { name: "loop" };
cycle.self = cycle;
let table = new Map([["k", new Set([1, 2])]]);
function helper() {}
class Thing {}
var pending = new Promise(() => {});
let nested = { fn: () => 1 };
var π = 3.14;
globalThis["my-key"] = 2;
globalThis[0] = "zero";
let café = "au lait";
let constructor = 5;
globalThis.parseFloat = null;
var process = { mine: true };
"#,
        )
        .run()
        .await
        .unwrap();

    let snapshot = session.snapshot_state().await.unwrap();
    let mut skipped = snapshot.skipped().to_vec();
    skipped.sort();
    assert_eq!(
        skipped,
        [
            "Thing",
            "helper",
            "nested",
            "parseFloat",
            "pending",
            "process"
        ]
    );

    let snapshot = eryx::StateSnapshot::from_bytes(&snapshot.to_bytes()).unwrap();
    let Some(mut fresh) = js_session().await else {
        return;
    };
    fresh.restore_state(&snapshot).await.unwrap();
    let out = stdout_of(
        &mut fresh,
        r#"
count += 1;
console.log(count, pair[0] === pair[1], pair[0] === shared, cycle.self === cycle);
console.log(shared.n === 1n << 70n, shared.when.getTime(), [...shared.bytes]);
console.log(table.get("k").has(2), typeof helper, typeof nested);
try { pair = 1 } catch (e) { console.log(e.name) }
console.log(π, globalThis["my-key"], globalThis[0], café, constructor, typeof parseFloat);
"#,
    )
    .await;
    assert_eq!(
        out,
        "42 true true true\ntrue 0 [1,2]\ntrue undefined undefined\nTypeError\n\
         3.14 2 zero au lait 5 function\n"
    );

    // Restored bindings are snapshotted again, and still can't be redeclared.
    let again = fresh.snapshot_state().await.unwrap();
    assert!(again.skipped().is_empty(), "{:?}", again.skipped());
    let Some(mut third) = js_session().await else {
        return;
    };
    third.restore_state(&again).await.unwrap();
    assert_eq!(
        stdout_of(&mut third, "console.log(count, table.size)").await,
        "42 1\n"
    );
    assert!(third.execute("let shared = 1").run().await.is_err());
}

/// A binding a failed script declared but never initialized can't be kept,
/// so it is named in `skipped` rather than dropped silently.
#[tokio::test]
async fn uninitialized_bindings_are_skipped() {
    let Some(mut session) = js_session().await else {
        return;
    };
    assert!(
        session
            .execute("var kept = 1; throw new Error('early'); let late = 2")
            .run()
            .await
            .is_err()
    );
    let snapshot = session.snapshot_state().await.unwrap();
    assert_eq!(snapshot.skipped(), ["late"]);
}

/// Clearing a session that has run code (the guest frees its whole QuickJS
/// runtime) leaves it working.
#[tokio::test]
async fn clear_state_resets_the_session() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session.execute("var x = 1; let y = 2").run().await.unwrap();
    session.clear_state().await.unwrap();
    assert_eq!(
        stdout_of(&mut session, "console.log(typeof x, typeof y)").await,
        "undefined undefined\n"
    );
}

/// A JavaScript snapshot names its state format, and a runtime refuses one
/// in another format with a distinct error rather than a parse failure.
#[tokio::test]
async fn snapshots_in_another_state_format_are_refused() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session.execute("var x = 1").run().await.unwrap();
    let bytes = session.snapshot_state().await.unwrap().to_bytes();

    const FORMAT: &[u8] = b"eryx-js-state/2 quickjs-ng@";
    let at = bytes
        .windows(FORMAT.len())
        .position(|w| w == FORMAT)
        .expect("the snapshot names its state format")
        + FORMAT.len();
    let mut other = bytes[..at].to_vec();
    other.extend_from_slice(b"0.0.0");
    let rest = &bytes[at..];
    let newline = rest.iter().position(|&b| b == b'\n').unwrap();
    other.extend_from_slice(&rest[newline..]);
    let other = eryx::StateSnapshot::from_bytes(&other).unwrap();

    let Some(mut fresh) = js_session().await else {
        return;
    };
    let err = fresh.restore_state(&other).await.unwrap_err();
    let ErrorKind::IncompatibleStateFormat {
        snapshot, runtime, ..
    } = err.kind()
    else {
        panic!("expected IncompatibleStateFormat, got {err:?}");
    };
    assert_eq!(snapshot, "eryx-js-state/2 quickjs-ng@0.0.0");
    assert!(
        runtime.starts_with("eryx-js-state/2 quickjs-ng@"),
        "{runtime}"
    );

    // Data with no format line names no format.
    let mut unnamed = bytes[..at - FORMAT.len()].to_vec();
    unnamed.extend_from_slice(b"no format line here");
    let unnamed = eryx::StateSnapshot::from_bytes(&unnamed).unwrap();
    let err = fresh.restore_state(&unnamed).await.unwrap_err();
    let ErrorKind::IncompatibleStateFormat { snapshot, .. } = err.kind() else {
        panic!("expected IncompatibleStateFormat, got {err:?}");
    };
    assert_eq!(snapshot, "");
}

/// Restoring a damaged snapshot (truncated, or with any byte changed) fails
/// with an error or restores plain values; it never traps the guest, which
/// keeps running.
#[tokio::test]
async fn damaged_snapshots_fail_cleanly() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session
        .execute(r#"var a = [1, "two", { three: 3n }]; let m = new Map([[/x+/g, new Date(5)]]);"#)
        .run()
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    let bytes = snapshot.to_bytes();
    // The guest's data is the end of the envelope; leave the format line be.
    let data_start = bytes.len() - snapshot.data().len();
    let body_start = data_start + snapshot.data().iter().position(|&b| b == b'\n').unwrap() + 1;

    // QuickJS's encoding starts with a version byte and a checksum, which
    // would reject nearly every change before the parser sees it; a
    // checksum of 0xffffffff is accepted for any data.
    let mut bytes = bytes;
    bytes[body_start + 1..body_start + 5].fill(0xff);
    let mut damaged = Vec::new();
    for len in body_start..bytes.len() {
        damaged.push(bytes[..len].to_vec());
    }
    for i in (body_start..bytes.len()).filter(|i| !(body_start + 1..body_start + 5).contains(i)) {
        for flip in [0x01, 0x80, 0xff] {
            let mut copy = bytes.clone();
            copy[i] ^= flip;
            damaged.push(copy);
        }
    }
    let Some(mut fresh) = js_session().await else {
        return;
    };
    let mut failures = 0;
    for copy in &damaged {
        let copy = eryx::StateSnapshot::from_bytes(copy).unwrap();
        match fresh.restore_state(&copy).await {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), ErrorKind::Snapshot(_)) => failures += 1,
            Err(e) => panic!("restoring {:?} failed in the guest: {e:?}", copy.data()),
        }
        assert_eq!(stdout_of(&mut fresh, "console.log(6 * 7)").await, "42\n");
    }
    assert!(failures > 0);
}

/// A session too big for a snapshot fails with an error and keeps running.
#[tokio::test]
async fn oversized_sessions_fail_cleanly() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session
        .execute("var ok = 1; var big = 'x'.repeat(11 * 1024 * 1024)")
        .run()
        .await
        .unwrap();
    let err = session.snapshot_state().await.unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
    assert_eq!(stdout_of(&mut session, "console.log(ok)").await, "1\n");
}

/// A snapshot whose body is `encoding`, after the format line and envelope of
/// a real one from this runtime.
async fn crafted_snapshot(encoding: &[u8]) -> eryx::StateSnapshot {
    let mut session = js_session().await.unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    let bytes = snapshot.to_bytes();
    let data_start = bytes.len() - snapshot.data().len();
    let body_start = data_start + snapshot.data().iter().position(|&b| b == b'\n').unwrap() + 1;
    let mut crafted = bytes[..body_start].to_vec();
    crafted.extend_from_slice(encoding);
    eryx::StateSnapshot::from_bytes(&crafted).unwrap()
}

/// QuickJS encoding header: version 28, a checksum that is accepted for
/// any data, and no atoms.
const HEADER: [u8; 6] = [28, 0xff, 0xff, 0xff, 0xff, 0];

/// Crafted encodings the parser must refuse: functions and modules
/// (bytecode), SharedArrayBuffers and out-of-range object references.
#[tokio::test]
async fn crafted_snapshots_are_refused() {
    let Some(mut session) = js_session().await else {
        return;
    };
    // Tags: 12 function bytecode, 13 module, 16 SharedArrayBuffer, 20
    // object reference (here to object 5 of 0); 9 is an array of length 1.
    for (what, body) in [
        ("bytecode", &[12, 0, 0, 0, 0][..]),
        ("module", &[13, 0, 0, 0][..]),
        ("shared array buffer", &[16, 4, 4, 0, 0, 0, 0][..]),
        ("object reference", &[9, 1, 20, 5][..]),
    ] {
        let mut encoding = HEADER.to_vec();
        encoding.extend_from_slice(body);
        let err = session
            .restore_state(&crafted_snapshot(&encoding).await)
            .await
            .unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Snapshot(_)),
            "{what}: {err:?}"
        );
        assert_eq!(
            stdout_of(&mut session, "console.log(6 * 7)").await,
            "42\n",
            "{what}"
        );
    }
}

/// A restore that traps the guest (here, an encoding nested too deep to
/// read) fails, and the session starts again from a fresh instance.
#[tokio::test]
async fn a_restore_that_traps_resets_the_session() {
    let Some(mut session) = js_session().await else {
        return;
    };
    session.execute("var before = 1").run().await.unwrap();
    let mut encoding = HEADER.to_vec();
    for _ in 0..1_000_000 {
        encoding.extend_from_slice(&[9, 1]);
    }
    encoding.push(1);
    let err = session
        .restore_state(&crafted_snapshot(&encoding).await)
        .await
        .unwrap_err();
    // A trap, not the guest's own error, which keeps the instance.
    assert!(
        matches!(err.kind(), ErrorKind::WasmEngine(m) if m.contains("wasm backtrace")),
        "{err:?}"
    );
    assert_eq!(
        stdout_of(&mut session, "console.log(typeof before, 6 * 7)").await,
        "undefined 42\n"
    );
}

/// The `process` the host sets for secrets is never session state; one the
/// script assigns itself isn't kept either, so it is named in `skipped`.
#[tokio::test]
async fn host_process_is_not_session_state() {
    use eryx::Session as _;

    let Some(builder) = js_sandbox() else {
        return;
    };
    let sandbox = builder
        .with_secret(
            "API_TOKEN",
            "real-secret-value",
            vec!["api.test".to_string()],
        )
        .build()
        .unwrap();
    let mut session = eryx::InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("var token = process.env.API_TOKEN.length > 0")
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    assert!(snapshot.skipped().is_empty(), "{:?}", snapshot.skipped());

    session.execute("process = { mine: true }").await.unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    assert_eq!(snapshot.skipped(), ["process"]);
}

/// Every callback call reports `callback_start` (its arguments as context)
/// and `callback_end`, as the CPython guest does; a failed call too. There
/// are no line-level events.
#[tokio::test]
async fn callbacks_report_trace_events() {
    use eryx::TraceEventKind;

    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder
        .with_callback(TestCallback::new("add", add))
        .with_callback(TestCallback::new("fail", |_| {
            Err(CallbackError::ExecutionFailed("nope".to_string()))
        }))
        .with_trace_collection(true)
        .build()
        .unwrap();

    let out = sandbox
        .execute("await add({ a: 1, b: 2 }); try { await fail() } catch {} await invoke('add', { a: 3 })")
        .await
        .unwrap();
    let events: Vec<_> = out
        .trace
        .iter()
        .map(|e| match &e.event {
            TraceEventKind::CallbackStart { name, .. } => (e.lineno, format!("start {name}")),
            TraceEventKind::CallbackEnd { name, .. } => (e.lineno, format!("end {name}")),
            other => panic!("unexpected trace event {other:?}"),
        })
        .collect();
    let expected = [
        "start add",
        "end add",
        "start fail",
        "end fail",
        "start add",
        "end add",
    ];
    assert_eq!(
        events,
        expected.map(|e| (0, e.to_string())),
        "{:?}",
        out.trace
    );
    let contexts: Vec<_> = out.trace.iter().map(|e| e.context.clone()).collect();
    assert_eq!(contexts[0], Some(json!({ "a": 1, "b": 2 })));
    assert_eq!(contexts[4], Some(json!({ "a": 3 })));
    assert_eq!(contexts[1], None);
}

/// An `invoke` still in flight when the script ends (here never awaited) is
/// cancelled and released by the guest: the execute returns, the session's
/// next execute (and callback) works, and, as in the CPython guest, the host
/// still runs the callback to the end, exactly once, and journals it, so a
/// replay serves it from the journal.
#[tokio::test]
async fn abandoned_invokes_are_released() {
    use eryx::Session as _;

    const NEVER_AWAITED: &str = "slow(); console.log('script end')";
    const HANG: Duration = Duration::from_secs(8);
    let slow = || {
        let mut slow = TestCallback::new("slow", |_| Ok(json!("slept")));
        slow.delay_ms = 300;
        slow
    };

    let Some(builder) = js_sandbox() else { return };
    let sandbox = builder.with_callback(slow()).build().unwrap();
    let mut session = eryx::InProcessSession::new(&sandbox).await.unwrap();
    for _ in 0..3 {
        let out = tokio::time::timeout(HANG, session.execute(NEVER_AWAITED))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.stdout, b"script end\n");
        let out = tokio::time::timeout(HANG, session.execute("console.log(await slow())"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.stdout, b"slept\n");
    }

    let callback = slow();
    let calls = Arc::clone(&callback.calls);
    let sandbox = js_sandbox()
        .unwrap()
        .with_callback(callback)
        .build()
        .unwrap();
    let first = tokio::time::timeout(HANG, sandbox.execute_with_journal(NEVER_AWAITED, None))
        .await
        .unwrap();
    first.result.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        first.journal.entries.len(),
        1,
        "{:?}",
        first.journal.entries
    );
    let second = tokio::time::timeout(
        HANG,
        sandbox.execute_with_journal(NEVER_AWAITED, Some(first.journal)),
    )
    .await
    .unwrap();
    second.result.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the replay re-ran the callback"
    );
    assert_eq!(second.replayed_callbacks, 1);
}
