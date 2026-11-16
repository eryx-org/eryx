//! Timers in the QuickJS guest (#923): `setTimeout` and friends,
//! `node:timers` and `node:timers/promises`, on the host's `sleep` import.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_timers
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use eryx::{DynamicCallback, Error, ErrorKind, ResourceLimits, Sandbox, SessionExecutor};
use serde_json::json;

/// `slow()`: a host callback that takes 50 ms.
fn slow() -> DynamicCallback {
    DynamicCallback::builder("slow", "Takes 50 ms", |_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(json!("slept"))
        })
    })
    .build()
}

fn sandbox(timeout: Duration) -> Sandbox {
    Sandbox::embedded()
        .javascript()
        .with_callback(slow())
        .with_resource_limits(ResourceLimits::default().with_execution_timeout(timeout))
        .build()
        .unwrap()
}

async fn run(code: &str) -> Result<String, Error> {
    sandbox(Duration::from_secs(10))
        .execute(code)
        .await
        .map(|out| out.stdout_text())
}

/// Timers fire in deadline order (first in, first out on a tie), after the
/// script's microtasks, and the execute waits for them without an `await`.
#[tokio::test]
async fn timers_fire_in_order_and_the_execute_waits() {
    let out = run(r"
        const log = [];
        setTimeout(() => log.push('b'), 20);
        setTimeout(() => log.push('a'), 10);
        setTimeout(() => log.push('c'), 20);
        setTimeout((x, y) => log.push(x + y), 15, 'ar', 'gs');
        setImmediate(() => log.push('immediate'));
        queueMicrotask(() => log.push('microtask'));
        setTimeout(() => {
            log.push('d');
            setTimeout(() => console.log([...log, 'nested'].join()), 5);
        }, 30);
        ")
    .await
    .unwrap();
    assert_eq!(out, "microtask,immediate,a,args,b,c,d,nested\n");
}

#[tokio::test]
async fn intervals_and_clearing() {
    let out = run(r"
        let ticks = 0;
        const interval = setInterval(() => {
            if (++ticks === 3) clearInterval(interval);
        }, 5);
        const never = setTimeout(() => console.log('never'), 10);
        clearTimeout(never);
        const byId = setTimeout(() => console.log('never either'), 10);
        clearTimeout(Number(byId));
        clearTimeout(undefined);
        const again = setTimeout(() => console.log('refreshed', ticks), 1);
        again.refresh();
        await new Promise((resolve) => setTimeout(resolve, 60));
        console.log(ticks, typeof interval.hasRef, interval.hasRef());
        ")
    .await
    .unwrap();
    assert_eq!(out, "refreshed 0\n3 function true\n");
}

/// An `unref()`'d timer doesn't keep the execute running, and is dropped
/// when it ends.
#[tokio::test]
async fn unref_timers_dont_keep_the_execute_running() {
    let start = Instant::now();
    let out = run("setTimeout(() => console.log('never'), 5000).unref(); console.log('done')")
        .await
        .unwrap();
    assert_eq!(out, "done\n");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

/// Waiting burns no fuel: a long wait costs about what a short one does.
#[tokio::test]
async fn waiting_burns_no_fuel() {
    let sandbox = sandbox(Duration::from_secs(10));
    let fuel = |out: eryx::ExecuteResult| out.stats.fuel_consumed.unwrap();
    let start = Instant::now();
    let long = fuel(sandbox.execute("setTimeout(() => {}, 300)").await.unwrap());
    assert!(start.elapsed() >= Duration::from_millis(300));
    let short = fuel(sandbox.execute("setTimeout(() => {}, 1)").await.unwrap());
    assert!(long < short * 2, "long {long}, short {short}");
}

/// The execution timeout covers the waiting: an interval nobody clears
/// runs until it fails the execute, as a Node process never exits.
#[tokio::test]
async fn an_endless_interval_hits_the_execution_timeout() {
    let start = Instant::now();
    let err = sandbox(Duration::from_millis(300))
        .execute("setInterval(() => {}, 10)")
        .await
        .unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Timeout(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

/// An exception in a timer callback fails the execute, and the other timers
/// never run.
#[tokio::test]
async fn a_throwing_callback_fails_the_execute() {
    let err = run(r"
        setTimeout(() => { throw new Error('boom'); }, 5);
        setTimeout(() => console.log('after'), 20);
        ")
    .await
    .unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    let stdout = err.stdout().unwrap_or_default();
    assert!(!String::from_utf8_lossy(stdout).contains("after"));
}

#[tokio::test]
async fn cancelling_stops_a_wait() {
    let sandbox = sandbox(Duration::from_secs(30));
    let start = Instant::now();
    let handle = sandbox.execute_cancellable("await new Promise((r) => setTimeout(r, 10000))");
    let token = handle.cancellation_token();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        token.cancel();
    });
    let err = handle.wait().await.unwrap_err();
    assert!(matches!(err.kind(), ErrorKind::Cancelled), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

/// Timers never outlive their execute, in a session too.
#[tokio::test]
async fn sessions_drop_pending_timers() {
    let executor = sandbox(Duration::from_secs(10)).executor();
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    let out = session
        .execute(
            "var late = setTimeout(() => console.log('late'), 50).unref(); console.log('first')",
        )
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "first\n");
    let out = session
        .execute("await new Promise((r) => setTimeout(r, 100)); console.log('second')")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "second\n");
    // A Timeout is a class instance, so a snapshot skips it.
    assert_eq!(session.snapshot_state().await.unwrap().skipped(), ["late"]);
}

#[tokio::test]
async fn timers_promises_and_abort_signals() {
    let out = run(r"
        import { setTimeout as sleep, setInterval as every } from 'node:timers/promises'
        import timers from 'timers'
        console.log(await sleep(5, 'value'), timers.setTimeout === setTimeout);
        const controller = new AbortController();
        const waiting = sleep(10000, 'x', { signal: controller.signal });
        setTimeout(() => controller.abort(), 10);
        try { await waiting; } catch (e) { console.log(e.name); }
        let n = 0;
        for await (const _ of every(5)) if (++n === 3) break;
        console.log(n);
        const signal = AbortSignal.timeout(10);
        signal.addEventListener('abort', () => console.log('timed out', signal.reason.name));
        await sleep(40);
        ")
    .await
    .unwrap();
    assert_eq!(out, "value true\nAbortError\n3\ntimed out TimeoutError\n");
}

/// Many timers share one host sleep, so they don't cost a host task each.
#[tokio::test]
async fn ten_thousand_timers() {
    let start = Instant::now();
    let out = run(r"
        let fired = 0;
        for (let i = 0; i < 10000; i++) setTimeout(() => fired++, i % 50);
        setTimeout(() => console.log(fired), 60);
        ")
    .await
    .unwrap();
    assert_eq!(out, "10000\n");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
}

/// Timers that are always due don't starve host callbacks: each pass runs
/// the timers due when it started, and the host gets a turn between passes.
#[tokio::test]
async fn due_timers_dont_starve_callbacks() {
    let start = Instant::now();
    let out = run(r"
        let done = false;
        slow().then(() => { done = true; });
        let spins = 0;
        while (!done) { spins++; await new Promise((r) => setImmediate(r)); }
        console.log('immediates', spins > 0);
        // An interval whose work outlasts its period, alongside a callback.
        const busy = setInterval(() => { const end = Date.now() + 3; while (Date.now() < end); }, 1);
        console.log(await slow());
        clearInterval(busy);
        ")
    .await
    .unwrap();
    assert_eq!(out, "immediates true\nslept\n");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

/// An invoke a timer starts keeps the execute running until it's done, as
/// I/O keeps a Node process running.
#[tokio::test]
async fn invokes_from_timers_keep_the_execute_running() {
    let out = run("setTimeout(async () => console.log('got', await slow()), 10)")
        .await
        .unwrap();
    assert_eq!(out, "got slept\n");
}

/// Only referenced timers (and invokes) keep a script that never settles
/// running; an unref'd one, like `AbortSignal.timeout()`'s, doesn't.
#[tokio::test]
async fn unref_timers_dont_keep_a_pending_script_running() {
    let start = Instant::now();
    let err = run("AbortSignal.timeout(2000); await new Promise(() => {})")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("execution never completed"),
        "{err}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

/// Refreshing a timer over and over doesn't grow the timer queue.
#[tokio::test]
async fn refresh_doesnt_grow_the_queue() {
    let sandbox = sandbox(Duration::from_secs(10));
    let peak = |refreshes: u32| {
        let sandbox = &sandbox;
        async move {
            let code = format!(
                "const t = setTimeout(() => {{}}, 10000); for (let i = 0; i < {refreshes}; i++) t.refresh(); clearTimeout(t)"
            );
            sandbox
                .execute(&code)
                .await
                .unwrap()
                .stats
                .peak_memory_bytes
                .unwrap()
        }
    };
    let few = peak(10).await;
    let many = peak(200_000).await;
    assert!(many < few + 4_000_000, "few {few}, many {many}");
}

/// A timed-out execute leaves no guest task behind, whether it was waiting
/// on a timer or spinning: the session's next execute starts on a fresh
/// instance and says so.
#[tokio::test]
async fn a_timed_out_session_starts_again() {
    for code in [
        "setInterval(() => {}, 10)",
        "await new Promise((r) => setTimeout(r, 10000))",
        "while (true) {}",
    ] {
        let executor = sandbox(Duration::from_secs(10)).executor();
        let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
        session.set_execution_timeout(Some(Duration::from_millis(300)));
        session.execute("var kept = 1").run().await.unwrap();
        let err = session.execute(code).run().await.unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Timeout(_)),
            "{code}: {err:?}"
        );
        let out = session
            .execute("console.log('next', typeof kept)")
            .run()
            .await
            .unwrap_or_else(|e| panic!("{code}: {e}"));
        assert_eq!(out.stdout_text(), "next undefined\n", "{code}");
        assert!(out.state_was_reset, "{code}");
    }
}

/// Which invokes the execute waits for: any started after the script's
/// synchronous run (in a promise callback, after an `await`, in a timer),
/// but not one its top-level code started and never awaited.
#[tokio::test]
async fn which_invokes_keep_the_execute_running() {
    for (code, expected) in [
        ("slow().then(console.log); console.log('end')", "end\n"),
        (
            "AbortSignal.timeout(10000); slow().then(console.log); console.log('end')",
            "end\n",
        ),
        (
            "Promise.resolve().then(() => slow().then(console.log))",
            "slept\n",
        ),
        ("await null; slow().then(console.log)", "slept\n"),
        ("setTimeout(() => slow().then(console.log), 1)", "slept\n"),
    ] {
        let start = Instant::now();
        assert_eq!(run(code).await.unwrap(), expected, "{code}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{code}: {:?}",
            start.elapsed()
        );
    }
}
