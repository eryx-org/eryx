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
#![allow(missing_docs, clippy::expect_used, clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eryx::JsonSchema;
use eryx::{CallbackError, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

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

/// A callback that echoes back its input.
struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
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

/// A callback that simulates work with a small delay.
struct WorkCallback;

impl TypedCallback for WorkCallback {
    type Args = WorkArgs;

    fn name(&self) -> &str {
        "work"
    }

    fn description(&self) -> &str {
        "Simulates async work"
    }

    fn invoke_typed(
        &self,
        args: WorkArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(args.ms)).await;
            Ok(json!({"slept_ms": args.ms}))
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
        eprintln!("Run `mise run build-eryx-runtime` first or set ERYX_WASM_PATH");
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

/// Benchmark sandbox creation with native extension caching.
///
/// Requires:
/// - `native-extensions` and `precompiled` features enabled
/// - numpy extracted at /tmp/numpy
///
/// Run with:
/// ```bash
/// cargo bench --package eryx --features native-extensions,precompiled -- caching
/// ```
#[cfg(all(feature = "native-extensions", feature = "precompiled"))]
fn bench_native_extension_caching(c: &mut Criterion) {
    use eryx::cache::InMemoryCache;
    use std::sync::Arc;

    let numpy_dir = std::path::Path::new("/tmp/numpy");
    if !numpy_dir.exists() {
        eprintln!("Skipping caching benchmarks: numpy not found at /tmp/numpy");
        eprintln!("Download it with:");
        eprintln!(
            "  curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz -o /tmp/numpy-wasi.tar.gz"
        );
        eprintln!("  tar -xzf /tmp/numpy-wasi.tar.gz -C /tmp/");
        return;
    }

    // Load extensions once
    let extensions: Vec<(String, Vec<u8>)> = walkdir::WalkDir::new(numpy_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "so"))
        .filter_map(|e| {
            let path = e.path();
            let numpy_parent = numpy_dir.parent()?;
            let relative_path = path.strip_prefix(numpy_parent).ok()?;
            let dlopen_path = format!("/site-packages/{}", relative_path.to_string_lossy());
            let bytes = std::fs::read(path).ok()?;
            Some((dlopen_path, bytes))
        })
        .collect();

    if extensions.is_empty() {
        eprintln!("No .so files found in /tmp/numpy");
        return;
    }

    // Get Python stdlib path
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let python_stdlib = std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("Could not find workspace root")
        .join("crates/eryx-wasm-runtime/tests/python-stdlib");

    let site_packages = numpy_dir.parent().expect("numpy parent");

    // Use cache_dir for mmap-based loading (fastest + lowest memory on Linux)
    let cache_dir = std::path::Path::new("/tmp/eryx-bench-cache");
    let _ = std::fs::remove_dir_all(cache_dir); // Clean for accurate benchmarks

    // In-memory cache for comparison (may be faster on some platforms)
    let memory_cache = Arc::new(InMemoryCache::new());

    let mut group = c.benchmark_group("native_extension_caching");
    group.sample_size(10); // Slower operations need fewer samples
    group.measurement_time(Duration::from_secs(30));

    // Cold start (no cache) - links + compiles + caches
    group.bench_function("cold_with_cache_dir", |b| {
        // Clean cache before each iteration for true cold start measurement
        let _ = std::fs::remove_dir_all(cache_dir);
        b.iter(|| {
            let mut builder = Sandbox::builder();
            for (name, bytes) in &extensions {
                builder = builder.with_native_extension(name.clone(), bytes.clone());
            }
            builder = builder
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .with_cache_dir(cache_dir)
                .expect("create cache dir");
            builder.build().expect("build sandbox")
        });
    });

    // Warm (mmap cache hit) - uses deserialize_file for fastest loading + lowest memory
    // First, populate the cache
    {
        let _ = std::fs::remove_dir_all(cache_dir);
        let mut builder = Sandbox::builder();
        for (name, bytes) in &extensions {
            builder = builder.with_native_extension(name.clone(), bytes.clone());
        }
        builder = builder
            .with_python_stdlib(&python_stdlib)
            .with_site_packages(site_packages)
            .with_cache_dir(cache_dir)
            .expect("create cache dir");
        let _ = builder.build().expect("populate cache");
    }

    group.bench_function("warm_mmap_cache", |b| {
        b.iter(|| {
            let mut builder = Sandbox::builder();
            for (name, bytes) in &extensions {
                builder = builder.with_native_extension(name.clone(), bytes.clone());
            }
            builder = builder
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .with_cache_dir(cache_dir)
                .expect("create cache dir");
            builder.build().expect("build sandbox")
        });
    });

    // Warm (in-memory cache hit) - for comparison on platforms where mmap may not help
    // First, populate the cache
    {
        let mut builder = Sandbox::builder();
        for (name, bytes) in &extensions {
            builder = builder.with_native_extension(name.clone(), bytes.clone());
        }
        builder = builder
            .with_python_stdlib(&python_stdlib)
            .with_site_packages(site_packages)
            .with_cache(memory_cache.clone());
        let _ = builder.build().expect("populate cache");
    }

    group.bench_function("warm_memory_cache", |b| {
        b.iter(|| {
            let mut builder = Sandbox::builder();
            for (name, bytes) in &extensions {
                builder = builder.with_native_extension(name.clone(), bytes.clone());
            }
            builder = builder
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .with_cache(memory_cache.clone());
            builder.build().expect("build sandbox")
        });
    });

    group.finish();

    // Clean up
    let _ = std::fs::remove_dir_all(cache_dir);
}

#[cfg(all(feature = "native-extensions", feature = "precompiled"))]
criterion_group!(
    benches,
    bench_sandbox_init,
    bench_wasm_instantiation,
    bench_simple_execution,
    bench_callback_invocation,
    bench_parallel_callbacks,
    bench_introspection,
    bench_native_extension_caching,
);

#[cfg(not(all(feature = "native-extensions", feature = "precompiled")))]
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
