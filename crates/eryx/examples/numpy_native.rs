// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

//! Example demonstrating native Python extension support with numpy.
//!
//! This example shows how to use late-linking to add numpy's native extensions
//! to the sandbox at creation time, and how caching dramatically improves
//! subsequent sandbox creation.
//!
//! # Prerequisites
//!
//! Download numpy from wasi-wheels:
//! ```bash
//! curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz \
//!     -o /tmp/numpy-wasi.tar.gz
//! tar -xzf /tmp/numpy-wasi.tar.gz -C /tmp/
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo run --example numpy_native --features native-extensions,precompiled --release
//! ```

use std::path::Path;
use std::time::Instant;

use eryx::Sandbox;

/// Load native extensions from a numpy directory.
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
            let numpy_parent = numpy_dir
                .parent()
                .ok_or("Cannot find numpy parent directory")?;
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
    // Path to extracted numpy from wasi-wheels
    let numpy_dir = Path::new("/tmp/numpy");

    if !numpy_dir.exists() {
        eprintln!("numpy not found at /tmp/numpy");
        eprintln!();
        eprintln!("Download it with:");
        eprintln!(
            "  curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz -o /tmp/numpy-wasi.tar.gz"
        );
        eprintln!("  tar -xzf /tmp/numpy-wasi.tar.gz -C /tmp/");
        return Ok(());
    }

    // Site packages directory (contains numpy Python files)
    let site_packages = numpy_dir
        .parent()
        .ok_or("Cannot find site-packages directory")?;

    println!("=== Numpy Native Extensions with Caching ===\n");

    // Load extensions once
    println!("Loading numpy native extensions...");
    let start = Instant::now();
    let extensions = load_numpy_extensions(numpy_dir)?;
    println!(
        "  Loaded {} extensions in {:?}",
        extensions.len(),
        start.elapsed()
    );

    // Create cache directory (using with_cache_dir for mmap-based loading)
    let cache_dir = Path::new("/tmp/eryx-cache");
    let _ = std::fs::remove_dir_all(cache_dir); // Clean for demo
    println!("  Cache directory: {}", cache_dir.display());

    // First sandbox creation (cold - no cache)
    println!("\n--- First sandbox (cache miss) ---\n");
    let start = Instant::now();

    // Use embedded() for runtime+stdlib, late-linking overrides when extensions present
    let mut builder = Sandbox::embedded();
    for (name, bytes) in &extensions {
        builder = builder.with_native_extension(name.clone(), bytes.clone());
    }
    let sandbox1 = builder
        .with_site_packages(site_packages)
        .with_cache_dir(cache_dir)?
        .build()?;
    let cold_time = start.elapsed();
    println!(
        "  Created in {:?} (cache miss - linked + compiled + cached)",
        cold_time
    );

    // Verify it works
    let result = sandbox1
        .execute("import numpy as np; print(np.array([1,2,3]).sum())")
        .await?;
    println!("  Test: {}", result.stdout.trim());

    // Second sandbox creation (warm - cache hit with mmap)
    println!("\n--- Second sandbox (cache hit) ---\n");
    let start = Instant::now();

    let mut builder = Sandbox::embedded();
    for (name, bytes) in &extensions {
        builder = builder.with_native_extension(name.clone(), bytes.clone());
    }
    let sandbox2 = builder
        .with_site_packages(site_packages)
        .with_cache_dir(cache_dir)?
        .build()?;
    let warm_time = start.elapsed();
    println!(
        "  Created in {:?} (cache hit - loaded precompiled)",
        warm_time
    );

    // Verify it works
    let result = sandbox2
        .execute("import numpy as np; print(np.array([4,5,6]).sum())")
        .await?;
    println!("  Test: {}", result.stdout.trim());

    // Third sandbox (also warm with mmap)
    println!("\n--- Third sandbox (cache hit) ---\n");
    let start = Instant::now();

    let mut builder = Sandbox::embedded();
    for (name, bytes) in &extensions {
        builder = builder.with_native_extension(name.clone(), bytes.clone());
    }
    let sandbox3 = builder
        .with_site_packages(site_packages)
        .with_cache_dir(cache_dir)?
        .build()?;
    let warm_time2 = start.elapsed();
    println!("  Created in {:?} (cache hit)", warm_time2);

    // Verify it works
    let result = sandbox3
        .execute("import numpy as np; print(np.linalg.det([[1,2],[3,4]]))")
        .await?;
    println!("  Test: det([[1,2],[3,4]]) = {}", result.stdout.trim());

    // Summary
    println!("\n=== Summary ===\n");
    println!("  Cold (cache miss): {:?}", cold_time);
    println!("  Warm (cache hit):  {:?}", warm_time);
    println!(
        "  Speedup: {:.1}x",
        cold_time.as_secs_f64() / warm_time.as_secs_f64()
    );

    // Check cache file
    let cache_files: Vec<_> = std::fs::read_dir(cache_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "cwasm"))
        .collect();

    if let Some(entry) = cache_files.first() {
        let metadata = entry.metadata()?;
        println!(
            "  Cache file: {} ({:.1} MB)",
            entry.file_name().to_string_lossy(),
            metadata.len() as f64 / 1_000_000.0
        );
    }

    println!("\n=== Full numpy test ===\n");

    let code = r#"
import numpy as np

# Basic array creation
a = np.array([1, 2, 3, 4, 5])
print(f"Array: {a}")
print(f"Sum: {a.sum()}")
print(f"Mean: {a.mean()}")

# Matrix operations
m = np.array([[1, 2], [3, 4]])
print(f"\nMatrix:\n{m}")
print(f"Determinant: {np.linalg.det(m):.1f}")

# Random numbers
rng = np.random.default_rng(42)
samples = rng.normal(0, 1, 1000)
print(f"\nRandom samples mean: {samples.mean():.4f}")
print(f"Random samples std: {samples.std():.4f}")

# Math functions
x = np.linspace(0, np.pi, 5)
print(f"\nsin values: {np.sin(x)}")

print("\nNumpy is working!")
"#;

    let result = sandbox3.execute(code).await?;
    println!("{}", result.stdout);

    Ok(())
}
