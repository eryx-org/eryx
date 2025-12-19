// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

//! Memory usage benchmark for multiple concurrent sandboxes.
//!
//! This benchmark measures RSS (Resident Set Size) to understand:
//! 1. Per-sandbox memory overhead
//! 2. Whether wasmtime shares compiled component bytes across instances
//! 3. Memory scaling with sandbox count
//!
//! # Running
//!
//! Base runtime (no numpy):
//! ```bash
//! cargo run --example memory_bench --features embedded --release
//! ```
//!
//! With numpy (requires native-extensions feature and numpy download):
//! ```bash
//! cargo run --example memory_bench --features native-extensions,embedded --release -- --numpy
//! ```

#[cfg(any(feature = "embedded", feature = "native-extensions"))]
use std::time::Instant;

#[cfg(any(feature = "embedded", feature = "native-extensions"))]
use eryx::Sandbox;

/// Get current process RSS (Resident Set Size) in MB.
#[cfg(any(feature = "embedded", feature = "native-extensions"))]
fn get_rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            let kb: f64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

/// Get current process virtual memory size in MB.
#[cfg(any(feature = "embedded", feature = "native-extensions"))]
fn get_vsz_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if line.starts_with("VmSize:") {
            let kb: f64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

#[cfg(feature = "native-extensions")]
fn load_numpy_extensions(
    numpy_dir: &std::path::Path,
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

#[cfg(any(feature = "embedded", feature = "native-extensions"))]
struct MemorySnapshot {
    rss_mb: f64,
    vsz_mb: f64,
}

#[cfg(any(feature = "embedded", feature = "native-extensions"))]
impl MemorySnapshot {
    fn now() -> Self {
        Self {
            rss_mb: get_rss_mb(),
            vsz_mb: get_vsz_mb(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let use_numpy = args.iter().any(|a| a == "--numpy");

    println!("=== Memory Usage Benchmark ===\n");

    if use_numpy {
        #[cfg(feature = "native-extensions")]
        {
            run_numpy_benchmark().await?;
        }
        #[cfg(not(feature = "native-extensions"))]
        {
            eprintln!("Error: --numpy requires native-extensions feature");
            eprintln!(
                "Run with: cargo run --example memory_bench --features native-extensions,embedded --release -- --numpy"
            );
        }
    } else {
        run_base_benchmark().await?;
    }

    Ok(())
}

#[cfg(feature = "embedded")]
async fn run_base_benchmark() -> Result<(), Box<dyn std::error::Error>> {
    println!("Mode: Base runtime (no numpy)\n");

    let baseline = MemorySnapshot::now();
    println!("Baseline:");
    println!("  RSS: {:.1} MB", baseline.rss_mb);
    println!("  VSZ: {:.1} MB\n", baseline.vsz_mb);

    // Create sandboxes and track memory
    let sandbox_counts = [1, 5, 10, 25, 50];
    let mut sandboxes: Vec<Sandbox> = Vec::new();
    let mut prev_rss = baseline.rss_mb;

    println!(
        "{:>8} {:>10} {:>12} {:>12} {:>10}",
        "Count", "RSS (MB)", "Delta (MB)", "Per-SB (MB)", "Time"
    );
    println!("{}", "-".repeat(56));

    for &target in &sandbox_counts {
        let start = Instant::now();
        while sandboxes.len() < target {
            let sandbox = Sandbox::embedded().build()?;
            sandboxes.push(sandbox);
        }
        let elapsed = start.elapsed();

        let snap = MemorySnapshot::now();
        let delta = snap.rss_mb - prev_rss;
        let added = target
            - if target == 1 {
                0
            } else {
                sandbox_counts[sandbox_counts.iter().position(|&x| x == target).unwrap() - 1]
            };
        let per_sandbox = if added > 0 {
            delta / added as f64
        } else {
            delta
        };

        println!(
            "{:>8} {:>10.1} {:>12.1} {:>12.2} {:>10.1?}",
            target, snap.rss_mb, delta, per_sandbox, elapsed
        );

        prev_rss = snap.rss_mb;
    }

    let final_snap = MemorySnapshot::now();
    println!("\nFinal:");
    println!("  RSS: {:.1} MB", final_snap.rss_mb);
    println!("  VSZ: {:.1} MB", final_snap.vsz_mb);
    println!(
        "  Total overhead: {:.1} MB for {} sandboxes",
        final_snap.rss_mb - baseline.rss_mb,
        sandboxes.len()
    );
    println!(
        "  Average per sandbox: {:.2} MB",
        (final_snap.rss_mb - baseline.rss_mb) / sandboxes.len() as f64
    );

    // Verify sandboxes work
    println!("\nVerifying sandboxes work...");
    let result = sandboxes[0].execute("print(1 + 1)").await?;
    println!("  Sandbox 0: {}", result.stdout.trim());

    Ok(())
}

#[cfg(not(feature = "embedded"))]
async fn run_base_benchmark() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Error: base benchmark requires embedded feature");
    eprintln!("Run with: cargo run --example memory_bench --features embedded --release");
    Ok(())
}

#[cfg(feature = "native-extensions")]
async fn run_numpy_benchmark() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::Path;

    let args: Vec<String> = std::env::args().collect();
    let use_mmap = args.iter().any(|a| a == "--mmap");

    if use_mmap {
        println!("Mode: With numpy (pre-initialized, MMAP loading)\n");
    } else {
        println!("Mode: With numpy (pre-initialized, bytes loading)\n");
    }

    let numpy_dir = Path::new("/tmp/numpy");
    if !numpy_dir.exists() {
        eprintln!("numpy not found at /tmp/numpy");
        eprintln!("Download with:");
        eprintln!(
            "  curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz -o /tmp/numpy-wasi.tar.gz"
        );
        eprintln!("  tar -xzf /tmp/numpy-wasi.tar.gz -C /tmp/");
        return Ok(());
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let python_stdlib = std::path::PathBuf::from(&manifest_dir)
        .parent()
        .ok_or("no parent")?
        .join("eryx-wasm-runtime/tests/python-stdlib");
    let site_packages = numpy_dir.parent().ok_or("no parent")?;

    let baseline = MemorySnapshot::now();
    println!("Baseline:");
    println!("  RSS: {:.1} MB", baseline.rss_mb);
    println!("  VSZ: {:.1} MB\n", baseline.vsz_mb);

    // Load and link extensions (one-time cost)
    println!("Loading numpy extensions...");
    let extensions = load_numpy_extensions(numpy_dir)?;
    let native_extensions: Vec<_> = extensions
        .iter()
        .map(|(name, bytes)| {
            eryx_runtime::linker::NativeExtension::new(name.clone(), bytes.clone())
        })
        .collect();

    let after_load = MemorySnapshot::now();
    println!("  Loaded {} extensions", extensions.len());
    println!(
        "  RSS: {:.1} MB (+{:.1} MB)\n",
        after_load.rss_mb,
        after_load.rss_mb - baseline.rss_mb
    );

    // Link extensions
    println!("Linking extensions...");
    let start = Instant::now();
    let linked = eryx_runtime::linker::link_with_extensions(&native_extensions)?;
    println!("  Linked in {:?}", start.elapsed());

    let after_link = MemorySnapshot::now();
    println!(
        "  RSS: {:.1} MB (+{:.1} MB)\n",
        after_link.rss_mb,
        after_link.rss_mb - after_load.rss_mb
    );

    // Pre-initialize with numpy
    println!("Pre-initializing with numpy...");
    let start = Instant::now();
    let preinit = eryx::preinit::pre_initialize(
        &python_stdlib,
        Some(site_packages),
        &["numpy"],
        &native_extensions,
    )
    .await?;
    println!("  Pre-init in {:?}", start.elapsed());

    let after_preinit = MemorySnapshot::now();
    println!(
        "  Preinit component: {:.1} MB",
        preinit.len() as f64 / 1_000_000.0
    );
    println!(
        "  RSS: {:.1} MB (+{:.1} MB)\n",
        after_preinit.rss_mb,
        after_preinit.rss_mb - after_link.rss_mb
    );

    // Precompile
    println!("Precompiling...");
    let start = Instant::now();
    let precompiled = eryx::PythonExecutor::precompile(&preinit)?;
    println!("  Precompiled in {:?}", start.elapsed());
    println!(
        "  Precompiled size: {:.1} MB",
        precompiled.len() as f64 / 1_000_000.0
    );

    let after_precompile = MemorySnapshot::now();
    println!(
        "  RSS: {:.1} MB (+{:.1} MB)\n",
        after_precompile.rss_mb,
        after_precompile.rss_mb - after_preinit.rss_mb
    );

    // For mmap test, save to file and drop from RAM
    let cwasm_path = Path::new("/tmp/eryx-memory-bench.cwasm");
    let precompiled_bytes = if use_mmap {
        println!("Saving precompiled to file for mmap loading...");
        std::fs::write(cwasm_path, &precompiled)?;
        println!("  Saved to {}\n", cwasm_path.display());
        None // Don't keep in RAM
    } else {
        Some(precompiled)
    };

    // Drop the preinit bytes to free memory
    drop(preinit);
    drop(linked);
    drop(native_extensions);
    drop(extensions);

    let after_cleanup = MemorySnapshot::now();
    if use_mmap {
        println!("After cleanup (precompiled saved to disk):");
    } else {
        println!("After cleanup (keeping only precompiled in RAM):");
    }
    println!("  RSS: {:.1} MB\n", after_cleanup.rss_mb);

    // Create sandboxes and track memory
    let sandbox_counts = if use_mmap {
        vec![1, 5, 10, 25, 50, 100] // Test more with mmap since memory is lower
    } else {
        vec![1, 5, 10, 25, 50]
    };
    let mut sandboxes: Vec<Sandbox> = Vec::new();
    let mut prev_rss = after_cleanup.rss_mb;

    println!(
        "{:>8} {:>10} {:>12} {:>12} {:>10}",
        "Count", "RSS (MB)", "Delta (MB)", "Per-SB (MB)", "Time"
    );
    println!("{}", "-".repeat(56));

    for &target in &sandbox_counts {
        let start = Instant::now();
        while sandboxes.len() < target {
            let sandbox = if use_mmap {
                unsafe {
                    Sandbox::builder()
                        .with_precompiled_file(cwasm_path)
                        .with_python_stdlib(&python_stdlib)
                        .with_site_packages(site_packages)
                        .build()?
                }
            } else {
                unsafe {
                    Sandbox::builder()
                        .with_precompiled_bytes(precompiled_bytes.as_ref().unwrap().clone())
                        .with_python_stdlib(&python_stdlib)
                        .with_site_packages(site_packages)
                        .build()?
                }
            };
            sandboxes.push(sandbox);
        }
        let elapsed = start.elapsed();

        let snap = MemorySnapshot::now();
        let delta = snap.rss_mb - prev_rss;
        let added = target
            - if target == 1 {
                0
            } else {
                sandbox_counts[sandbox_counts.iter().position(|&x| x == target).unwrap() - 1]
            };
        let per_sandbox = if added > 0 {
            delta / added as f64
        } else {
            delta
        };

        println!(
            "{:>8} {:>10.1} {:>12.1} {:>12.2} {:>10.1?}",
            target, snap.rss_mb, delta, per_sandbox, elapsed
        );

        prev_rss = snap.rss_mb;
    }

    let final_snap = MemorySnapshot::now();
    println!("\nFinal:");
    println!("  RSS: {:.1} MB", final_snap.rss_mb);
    println!("  VSZ: {:.1} MB", final_snap.vsz_mb);
    println!(
        "  Total overhead: {:.1} MB for {} sandboxes",
        final_snap.rss_mb - after_cleanup.rss_mb,
        sandboxes.len()
    );
    println!(
        "  Average per sandbox: {:.2} MB",
        (final_snap.rss_mb - after_cleanup.rss_mb) / sandboxes.len() as f64
    );

    // Verify sandboxes work
    println!("\nVerifying sandboxes work...");
    let result = sandboxes[0]
        .execute("import numpy as np; print(np.array([1,2,3]).sum())")
        .await?;
    println!("  Sandbox 0 (numpy sum): {}", result.stdout.trim());

    Ok(())
}
