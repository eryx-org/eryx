//! Benchmarks for measuring sandbox execution overhead.
//!
//! Run with: `cargo bench --package eryx`
//!
//! These benchmarks measure:
//! - WASM component initialization time (slow - few samples)
//! - WASM instantiation overhead (per-execution cost)
//! - Simple code execution overhead
//! - Callback invocation overhead
//! - Parallel callback execution performance

// Benchmarks use expect/unwrap for simplicity - failures should panic
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::needless_raw_string_hashes)]
// Callback trait requires returning &str with lifetime tied to &self
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::redundant_closure_for_method_calls)]

use std::future::Future;
use std::pin::Pin;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eryx::{Callback, CallbackError, Sandbox};
use serde_json::{Value, json};
use std::time::Duration;

/// A no-op callback that returns immediately.
struct NoopCallback;

impl Callback for NoopCallback {
    fn name(&self) -> &str {
        "noop"
    }

    fn description(&self) -> &str {
        "A no-op callback that returns immediately"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn invoke(
        &self,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"ok": true})) })
    }
}

/// A callback that echoes back its input.
struct EchoCallback;

impl Callback for EchoCallback {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "data": { "description": "Data to echo back" }
            },
            "required": ["data"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let data = args.get("data").cloned().unwrap_or_default();
            Ok(data)
        })
    }
}

/// A callback that simulates work with a small delay.
struct WorkCallback;

impl Callback for WorkCallback {
    fn name(&self) -> &str {
        "work"
    }

    fn description(&self) -> &str {
        "Simulates async work"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ms": { "type": "number", "description": "Milliseconds to sleep" }
            },
            "required": ["ms"]
        })
    }

    fn invoke(
        &self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(1);
            tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
            Ok(json!({"slept_ms": ms}))
        })
    }
}

fn get_wasm_path() -> String {
    std::env::var("ERYX_WASM_PATH").unwrap_or_else(|_| {
        // When running benchmarks, the working directory is the workspace root,
        // but CARGO_MANIFEST_DIR points to the crate directory.
        // We need to go up two levels from the crate to reach the workspace root.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = std::path::Path::new(manifest_dir)
            .parent()
            .and_then(|p| p.parent())
            .expect("Could not find workspace root");
        workspace_root
            .join("crates/eryx-runtime/runtime.wasm")
            .to_string_lossy()
            .into_owned()
    })
}

fn create_sandbox() -> Sandbox {
    Sandbox::builder()
        .with_wasm_file(get_wasm_path())
        .with_callback(NoopCallback)
        .with_callback(EchoCallback)
        .with_callback(WorkCallback)
        .build()
        .expect("Failed to create sandbox")
}

/// Benchmark sandbox initialization time.
/// This is the only benchmark that measures sandbox creation itself.
/// Note: This is inherently slow (~1-2s per iteration) due to WASM compilation.
fn bench_sandbox_init(c: &mut Criterion) {
    let wasm_path = get_wasm_path();

    // Skip if WASM file doesn't exist
    if !std::path::Path::new(&wasm_path).exists() {
        eprintln!("Skipping benchmarks: WASM file not found at {wasm_path}");
        eprintln!("Run `mise run build-wasm` first or set ERYX_WASM_PATH");
        return;
    }

    // Configure for slow benchmarks: fewer samples, longer measurement time
    let mut group = c.benchmark_group("sandbox_init");
    group.sample_size(10); // Minimum allowed by criterion
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("create", |b| {
        b.iter(|| {
            let _sandbox = Sandbox::builder()
                .with_wasm_file(&wasm_path)
                .with_callback(NoopCallback)
                .build()
                .expect("Failed to create sandbox");
        });
    });

    group.finish();
}

/// Benchmark raw WASM instantiation time (without full Sandbox overhead).
/// This isolates the cost of creating a WASM instance from the pre-compiled template.
fn bench_wasm_instantiation(c: &mut Criterion) {
    let wasm_path = get_wasm_path();
    if !std::path::Path::new(&wasm_path).exists() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Create sandbox once to get access to its internals
    // We're measuring how fast execute() can be called repeatedly
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("wasm_instantiation");

    // Benchmark the minimal execution path
    group.bench_function("minimal_pass", |b| {
        b.to_async(&rt)
            .iter(|| async { sandbox.execute("pass").await.expect("Execution failed") });
    });

    // Compare with a slightly more complex but still minimal operation
    group.bench_function("minimal_none", |b| {
        b.to_async(&rt)
            .iter(|| async { sandbox.execute("None").await.expect("Execution failed") });
    });

    // Measure just assignment (no function calls)
    group.bench_function("assignment", |b| {
        b.to_async(&rt)
            .iter(|| async { sandbox.execute("x = 1").await.expect("Execution failed") });
    });

    group.finish();
}

/// Benchmark simple Python code execution (no callbacks).
/// Reuses a single sandbox instance across all iterations.
fn bench_simple_execution(c: &mut Criterion) {
    let wasm_path = get_wasm_path();
    if !std::path::Path::new(&wasm_path).exists() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Create sandbox once, reuse for all benchmarks in this group
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("simple_execution");

    // Empty code
    group.bench_function("empty", |b| {
        b.to_async(&rt)
            .iter(|| async { sandbox.execute("pass").await.expect("Execution failed") });
    });

    // Simple print
    group.bench_function("print", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute("print('hello')")
                .await
                .expect("Execution failed")
        });
    });

    // Simple arithmetic
    group.bench_function("arithmetic", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute("x = 2 + 2 * 3")
                .await
                .expect("Execution failed")
        });
    });

    // Loop
    group.bench_function("loop_100", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute("total = sum(range(100))")
                .await
                .expect("Execution failed")
        });
    });

    // String operations
    group.bench_function("string_ops", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute(r#"s = "hello " * 10; s = s.upper()"#)
                .await
                .expect("Execution failed")
        });
    });

    group.finish();
}

/// Benchmark callback invocation overhead.
/// Reuses a single sandbox instance across all iterations.
fn bench_callback_invocation(c: &mut Criterion) {
    let wasm_path = get_wasm_path();
    if !std::path::Path::new(&wasm_path).exists() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Create sandbox once, reuse for all benchmarks in this group
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("callback_invocation");

    // Single noop callback
    group.bench_function("single_noop", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute(r#"result = await invoke("noop", "{}")"#)
                .await
                .expect("Execution failed")
        });
    });

    // Single echo callback with small data
    group.bench_function("single_echo_small", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute(r#"result = await invoke("echo", '{"data": "hello"}')"#)
                .await
                .expect("Execution failed")
        });
    });

    // Single echo callback with larger data
    group.bench_function("single_echo_large", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute(r#"result = await invoke("echo", '{"data": "' + 'x' * 1000 + '"}')"#)
                .await
                .expect("Execution failed")
        });
    });

    // Multiple sequential callbacks
    for count in [2, 5, 10] {
        group.bench_with_input(
            BenchmarkId::new("sequential", count),
            &count,
            |b, &count| {
                let code = format!(
                    "
for _ in range({count}):
    await invoke(\"noop\", \"{{}}\")
"
                );
                b.to_async(&rt)
                    .iter(|| async { sandbox.execute(&code).await.expect("Execution failed") });
            },
        );
    }

    group.finish();
}

/// Benchmark parallel callback execution.
/// Reuses a single sandbox instance across all iterations.
fn bench_parallel_callbacks(c: &mut Criterion) {
    let wasm_path = get_wasm_path();
    if !std::path::Path::new(&wasm_path).exists() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Create sandbox once, reuse for all benchmarks in this group
    let sandbox = create_sandbox();

    let mut group = c.benchmark_group("parallel_callbacks");

    // Parallel noop callbacks
    for count in [2, 5, 10] {
        group.bench_with_input(
            BenchmarkId::new("parallel_noop", count),
            &count,
            |b, &count| {
                let invokes = (0..count)
                    .map(|_| "invoke(\"noop\", \"{}\")")
                    .collect::<Vec<_>>()
                    .join(", ");
                let code = format!(
                    "
import asyncio
results = await asyncio.gather({invokes})
"
                );
                b.to_async(&rt)
                    .iter(|| async { sandbox.execute(&code).await.expect("Execution failed") });
            },
        );
    }

    // Parallel with small delay (tests actual parallelism)
    // Using 10ms delay to show parallelism benefit
    for count in [2, 3, 5] {
        group.bench_with_input(
            BenchmarkId::new("parallel_10ms_delay", count),
            &count,
            |b, &count| {
                let invokes = (0..count)
                    .map(|_| "invoke(\"work\", '{\"ms\": 10}')")
                    .collect::<Vec<_>>()
                    .join(", ");
                let code = format!(
                    "
import asyncio
results = await asyncio.gather({invokes})
"
                );
                b.to_async(&rt)
                    .iter(|| async { sandbox.execute(&code).await.expect("Execution failed") });
            },
        );
    }

    group.finish();
}

/// Benchmark list_callbacks introspection.
/// Reuses a single sandbox instance across all iterations.
fn bench_introspection(c: &mut Criterion) {
    let wasm_path = get_wasm_path();
    if !std::path::Path::new(&wasm_path).exists() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();

    // Create sandbox once, reuse for all benchmarks
    let sandbox = create_sandbox();

    c.bench_function("list_callbacks", |b| {
        b.to_async(&rt).iter(|| async {
            sandbox
                .execute("callbacks = list_callbacks()")
                .await
                .expect("Execution failed")
        });
    });
}

criterion_group!(
    benches,
    bench_sandbox_init,
    bench_wasm_instantiation,
    bench_simple_execution,
    bench_callback_invocation,
    bench_parallel_callbacks,
    bench_introspection,
);

criterion_main!(benches);
