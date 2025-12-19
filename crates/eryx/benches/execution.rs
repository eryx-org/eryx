//! Benchmarks for measuring sandbox and session execution overhead.
//!
//! Run with: `cargo bench --package eryx --features embedded`
//!
//! ## Benchmark Groups
//!
//! - **sandbox_creation**: Measures time to create a new sandbox
//! - **stateless_execution**: Measures `Sandbox::execute()` which creates a fresh WASM instance each time
//! - **session_execution**: Measures `InProcessSession::execute()` which reuses the WASM instance
//! - **callback_overhead**: Measures callback invocation overhead
//! - **parallel_callbacks**: Measures concurrent callback performance

// Benchmarks use expect/unwrap for simplicity - failures should panic
// RefCell across await is fine here - single-threaded benchmark code
#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::await_holding_refcell_ref
)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use eryx::session::{InProcessSession, Session};
use eryx::{CallbackError, JsonSchema, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

// ============================================================================
// Callbacks for testing
// ============================================================================

/// A no-op callback that returns immediately.
struct NoopCallback;

impl TypedCallback for NoopCallback {
    type Args = ();

    fn name(&self) -> &str {
        "noop"
    }

    fn description(&self) -> &str {
        "A no-op callback that returns immediately"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"ok": true})) })
    }
}

/// Arguments for the echo callback.
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// Data to echo back
    data: Value,
}

/// A callback that echoes its input.
struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input data back"
    }

    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(args.data) })
    }
}

/// Arguments for the work callback.
#[derive(Deserialize, JsonSchema)]
struct WorkArgs {
    /// Milliseconds to sleep
    ms: u64,
}

/// A callback that simulates work by sleeping.
struct WorkCallback;

impl TypedCallback for WorkCallback {
    type Args = WorkArgs;

    fn name(&self) -> &str {
        "work"
    }

    fn description(&self) -> &str {
        "Simulates work by sleeping for the specified duration"
    }

    fn invoke_typed(
        &self,
        args: WorkArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(args.ms)).await;
            Ok(json!({"slept_ms": args.ms}))
        })
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn create_sandbox() -> Sandbox {
    Sandbox::embedded()
        .with_callback(NoopCallback)
        .with_callback(EchoCallback)
        .with_callback(WorkCallback)
        .build()
        .expect("Failed to create sandbox")
}

// ============================================================================
// Sandbox Creation Benchmarks
// ============================================================================

/// Benchmark sandbox creation time.
///
/// With InstancePreCache, subsequent creations should be nearly instant (~1μs).
fn bench_sandbox_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("sandbox_creation");

    // First, ensure the cache is warm
    let _ = create_sandbox();

    // Measure cached sandbox creation (should be ~1μs with InstancePreCache)
    group.bench_function("cached", |b| {
        b.iter(|| {
            let _sandbox = create_sandbox();
        });
    });

    group.finish();
}

// ============================================================================
// Stateless Execution Benchmarks (fresh WASM instance each time)
// ============================================================================

/// Benchmark stateless execution via `Sandbox::execute()`.
///
/// Each call creates a fresh WASM instance and initializes Python from scratch.
/// This is ~500ms per execution due to Python interpreter initialization.
///
/// Use this when you need complete isolation between executions.
fn bench_stateless_execution(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("stateless_execution");

    // Stateless execution is slow (~500ms), so reduce sample size
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("pass", |b| {
        b.to_async(&rt)
            .iter(|| async { sandbox.execute("pass").await.expect("Execution failed") });
    });

    group.bench_function("print", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute("print('hello')")
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

// ============================================================================
// Session Execution Benchmarks (reused WASM instance)
// ============================================================================

/// Benchmark session-based execution via `InProcessSession::execute()`.
///
/// The WASM instance is kept alive between executions, so Python init
/// only happens once. Subsequent executions are MUCH faster (~1-5ms).
///
/// Use this for REPL-style interactions or when you need state persistence.
fn bench_session_execution(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    // Create session once (this does the Python init)
    let session = rt
        .block_on(InProcessSession::new(&sandbox))
        .expect("Failed to create session");

    // We need interior mutability for the session since bench_function takes &self
    let session = std::cell::RefCell::new(session);

    let mut group = c.benchmark_group("session_execution");

    // Session execution is fast, use default sample size
    group.bench_function("pass", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("pass")
                .await
                .expect("Execution failed")
        });
    });

    group.bench_function("print", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("print('hello')")
                .await
                .expect("Execution failed")
        });
    });

    group.bench_function("arithmetic", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("x = 2 + 2 * 3")
                .await
                .expect("Execution failed")
        });
    });

    group.bench_function("loop_100", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("total = sum(range(100))")
                .await
                .expect("Execution failed")
        });
    });

    group.bench_function("string_ops", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(r#"s = "hello " * 10; s = s.upper()"#)
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

// ============================================================================
// Session Creation Benchmarks
// ============================================================================

/// Benchmark session creation time.
///
/// This measures the time to create a new InProcessSession, which includes
/// WASM instantiation and Python interpreter initialization.
fn bench_session_creation(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("session_creation");

    // Session creation includes Python init, so it's slow
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("new", |b| {
        b.to_async(&rt).iter(|| async {
            let _session = InProcessSession::new(&sandbox)
                .await
                .expect("Failed to create session");
        });
    });

    group.finish();
}

// ============================================================================
// Callback Benchmarks
// ============================================================================

/// Benchmark callback invocation overhead using sessions.
fn bench_callback_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    let session = rt
        .block_on(InProcessSession::new(&sandbox))
        .expect("Failed to create session");
    let session = std::cell::RefCell::new(session);

    let mut group = c.benchmark_group("callback_overhead");

    // Single no-op callback
    group.bench_function("noop_single", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("result = noop()")
                .await
                .expect("Execution failed")
        });
    });

    // Multiple no-op callbacks in sequence
    group.bench_function("noop_10x", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(
                    r#"
for _ in range(10):
    noop()
"#,
                )
                .await
                .expect("Execution failed")
        });
    });

    // Echo callback with data
    group.bench_function("echo_small", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(r#"result = echo(data={"key": "value"})"#)
                .await
                .expect("Execution failed")
        });
    });

    // Echo callback with larger data
    group.bench_function("echo_large", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(r#"result = echo(data={"items": list(range(100))})"#)
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

/// Benchmark parallel callback execution.
fn bench_parallel_callbacks(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    let session = rt
        .block_on(InProcessSession::new(&sandbox))
        .expect("Failed to create session");
    let session = std::cell::RefCell::new(session);

    let mut group = c.benchmark_group("parallel_callbacks");

    // Sequential work callbacks (baseline)
    group.bench_function("sequential_5x10ms", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(
                    r#"
for _ in range(5):
    work(ms=10)
"#,
                )
                .await
                .expect("Execution failed")
        });
    });

    // Parallel work callbacks using asyncio
    // Note: Use top-level await since Python is already in an async context
    // The work() callback returns a coroutine, so we can gather them
    group.bench_function("parallel_5x10ms", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute(
                    r#"
import asyncio
tasks = [work(ms=10) for _ in range(5)]
await asyncio.gather(*tasks)
"#,
                )
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

// ============================================================================
// Introspection Benchmarks
// ============================================================================

/// Benchmark callback introspection (listing available callbacks).
fn bench_introspection(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sandbox = create_sandbox();

    let session = rt
        .block_on(InProcessSession::new(&sandbox))
        .expect("Failed to create session");
    let session = std::cell::RefCell::new(session);

    let mut group = c.benchmark_group("introspection");

    group.bench_function("list_callbacks", |b| {
        b.to_async(&rt).iter(|| async {
            session
                .borrow_mut()
                .execute("callbacks = list_callbacks()")
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

// ============================================================================
// Criterion Configuration
// ============================================================================

criterion_group!(
    benches,
    bench_sandbox_creation,
    bench_session_creation,
    bench_stateless_execution,
    bench_session_execution,
    bench_callback_overhead,
    bench_parallel_callbacks,
    bench_introspection,
);

criterion_main!(benches);
