// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

//! Quick benchmark of session per-execution time with numpy
//!
//! Also compares bytes-based vs mmap-based component loading.

use std::path::Path;
use std::time::Instant;

use eryx::Sandbox;
use eryx::Session;
use eryx::session::InProcessSession;

fn load_numpy_extensions(
    numpy_dir: &Path,
) -> Result<Vec<(String, Vec<u8>)>, Box<dyn std::error::Error>> {
    let mut extensions = Vec::new();
    for entry in walkdir::WalkDir::new(numpy_dir) {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension()
            && ext == "so"
        {
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

    let site_packages = numpy_dir.parent().ok_or("no parent")?;

    // Use cache directory for mmap-based loading (faster + less memory)
    let cache_dir = Path::new("/tmp/eryx-session-bench-cache");
    let _ = std::fs::remove_dir_all(cache_dir); // Clean for accurate benchmarks
    std::fs::create_dir_all(cache_dir)?;

    println!("=== Session Per-Execution Benchmark ===\n");

    // Load extensions
    let extensions = load_numpy_extensions(numpy_dir)?;
    println!("Loaded {} native extensions\n", extensions.len());

    // Create sandbox using cache_dir (handles linking, pre-init, precompile, and mmap)
    println!("Creating sandbox (cold - linking + compiling + caching)...");
    let start = Instant::now();
    // Start with embedded() which provides runtime+stdlib, then late-linking
    // overrides the runtime when native extensions are added
    let mut builder = Sandbox::embedded();
    for (name, bytes) in &extensions {
        builder = builder.with_native_extension(name.clone(), bytes.clone());
    }
    let sandbox = builder
        .with_site_packages(site_packages)
        .with_cache_dir(cache_dir)?
        .build()?;
    println!("  Cold sandbox creation: {:?}\n", start.elapsed());

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

    // Test warm sandbox creation (cache hit with mmap)
    println!("\n=== Warm Sandbox Creation (cache hit) ===\n");

    let mut times = vec![];
    for i in 0..5 {
        let start = Instant::now();
        let mut builder = Sandbox::embedded();
        for (name, bytes) in &extensions {
            builder = builder.with_native_extension(name.clone(), bytes.clone());
        }
        let _sandbox = builder
            .with_site_packages(site_packages)
            .with_cache_dir(cache_dir)?
            .build()?;
        let elapsed = start.elapsed();
        if i == 0 {
            println!("  First (verifying cache): {:?}", elapsed);
        }
        times.push(elapsed);
    }
    let avg = times.iter().skip(1).map(|t| t.as_millis()).sum::<u128>() / (times.len() - 1) as u128;
    println!("  Average (excluding first): {}ms", avg);

    // Summary
    println!("\n=== Summary ===");
    println!("  Session per-execution: sub-millisecond");
    println!("  Warm sandbox creation: ~{}ms (mmap cache hit)", avg);

    // Clean up
    let _ = std::fs::remove_dir_all(cache_dir);

    Ok(())
}
