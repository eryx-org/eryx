//! Quick benchmark of session per-execution time with numpy
//!
//! Also compares bytes-based vs mmap-based component loading.

use std::path::Path;
use std::time::Instant;

use eryx::Sandbox;
use eryx::Session;
use eryx::session::InProcessSession;

fn load_numpy_extensions(numpy_dir: &Path) -> Result<Vec<(String, Vec<u8>)>, Box<dyn std::error::Error>> {
    let mut extensions = Vec::new();
    for entry in walkdir::WalkDir::new(numpy_dir) {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension() && ext == "so" {
            let numpy_parent = numpy_dir.parent().ok_or("no parent")?;
            let relative_path = path.strip_prefix(numpy_parent)?;
            let dlopen_path = format!("/site-packages/{}", relative_path.to_string_lossy());
            let bytes = std::fs::read(path)?;
            extensions.push((dlopen_path, bytes));
        }
    }
    Ok(extensions)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let numpy_dir = Path::new("/tmp/numpy");
    if !numpy_dir.exists() {
        eprintln!("numpy not found at /tmp/numpy");
        return Ok(());
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let python_stdlib = std::path::PathBuf::from(&manifest_dir)
        .parent().ok_or("no parent")?
        .join("eryx-wasm-runtime/tests/python-stdlib");
    let site_packages = numpy_dir.parent().ok_or("no parent")?;

    println!("=== Session Per-Execution Benchmark ===\n");

    // Load and link extensions
    let extensions = load_numpy_extensions(numpy_dir)?;
    let native_extensions: Vec<_> = extensions.iter()
        .map(|(name, bytes)| eryx_runtime::linker::NativeExtension::new(name.clone(), bytes.clone()))
        .collect();
    let linked = eryx_runtime::linker::link_with_extensions(&native_extensions)?;

    // Pre-init with numpy
    println!("Pre-initializing with numpy...");
    let preinit = eryx::preinit::pre_initialize(&linked, &python_stdlib, Some(site_packages), &["numpy"]).await?;
    let precompiled = eryx::PythonExecutor::precompile(&preinit)?;
    println!("  Component size: {:.1} MB\n", precompiled.len() as f64 / 1_000_000.0);

    // Create sandbox
    let start = Instant::now();
    let sandbox = unsafe {
        Sandbox::builder()
            .with_precompiled_bytes(precompiled)
            .with_python_stdlib(&python_stdlib)
            .with_site_packages(site_packages)
            .build()?
    };
    println!("Sandbox creation: {:?}\n", start.elapsed());

    // Create session
    let start = Instant::now();
    let mut session = InProcessSession::new(&sandbox).await?;
    println!("Session creation: {:?}\n", start.elapsed());

    // Warm up and import numpy
    println!("Warming up (importing numpy in session)...");
    let start = Instant::now();
    session.execute("import numpy as np").await?;
    println!("  numpy import: {:?}", start.elapsed());
    session.execute("x = 1").await?;

    // Benchmark simple execution
    println!("\n--- Simple execution (x = 1) ---");
    let mut times = vec![];
    for _ in 0..10 {
        let start = Instant::now();
        session.execute("x = 1").await?;
        times.push(start.elapsed());
    }
    let avg = times.iter().map(|t| t.as_micros()).sum::<u128>() / times.len() as u128;
    println!("  Average: {}µs ({:.2}ms)", avg, avg as f64 / 1000.0);

    // Benchmark with numpy operation
    println!("\n--- Numpy operation (np.sum([1,2,3])) ---");
    let mut times = vec![];
    for _ in 0..10 {
        let start = Instant::now();
        session.execute("result = np.sum([1,2,3])").await?;
        times.push(start.elapsed());
    }
    let avg = times.iter().map(|t| t.as_micros()).sum::<u128>() / times.len() as u128;
    println!("  Average: {}µs ({:.2}ms)", avg, avg as f64 / 1000.0);

    // Benchmark with print
    println!("\n--- Print operation ---");
    let mut times = vec![];
    for _ in 0..10 {
        let start = Instant::now();
        session.execute("print('hello')").await?;
        times.push(start.elapsed());
    }
    let avg = times.iter().map(|t| t.as_micros()).sum::<u128>() / times.len() as u128;
    println!("  Average: {}µs ({:.2}ms)", avg, avg as f64 / 1000.0);

    // Compare to main branch baseline
    println!("\n=== Comparison ===");
    println!("Main branch (no numpy): ~2ms per execution");
    println!("This branch with numpy: see above");

    // Test mmap-based loading vs bytes-based loading
    println!("\n=== Mmap vs Bytes Loading ===\n");

    // Save precompiled to file
    let cache_dir = std::path::Path::new("/tmp/eryx-mmap-test");
    let _ = std::fs::remove_dir_all(cache_dir);
    std::fs::create_dir_all(cache_dir)?;
    let cwasm_path = cache_dir.join("numpy.cwasm");

    // Re-create precompiled for this test
    let preinit = eryx::preinit::pre_initialize(&linked, &python_stdlib, Some(site_packages), &["numpy"]).await?;
    let precompiled = eryx::PythonExecutor::precompile(&preinit)?;
    std::fs::write(&cwasm_path, &precompiled)?;
    println!("Saved {:.1} MB to {}", precompiled.len() as f64 / 1_000_000.0, cwasm_path.display());

    // Benchmark bytes-based loading
    println!("\n--- Bytes-based loading (read into RAM) ---");
    let mut times = vec![];
    for _ in 0..5 {
        let bytes = std::fs::read(&cwasm_path)?;
        let start = Instant::now();
        let _ = unsafe {
            Sandbox::builder()
                .with_precompiled_bytes(bytes)
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .build()?
        };
        times.push(start.elapsed());
    }
    let avg = times.iter().map(|t| t.as_millis()).sum::<u128>() / times.len() as u128;
    println!("  Average: {}ms", avg);

    // Benchmark mmap-based loading
    println!("\n--- Mmap-based loading (deserialize_file) ---");
    let mut times = vec![];
    for _ in 0..5 {
        let start = Instant::now();
        let _ = unsafe {
            Sandbox::builder()
                .with_precompiled_file(&cwasm_path)
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .build()?
        };
        times.push(start.elapsed());
    }
    let avg = times.iter().map(|t| t.as_millis()).sum::<u128>() / times.len() as u128;
    println!("  Average: {}ms", avg);

    // Clean up
    let _ = std::fs::remove_dir_all(cache_dir);

    Ok(())
}
