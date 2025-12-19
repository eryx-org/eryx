// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Example demonstrating the with_package() API for loading Python packages.
//!
//! This shows how to load packages from:
//! - tar.gz archives (wasi-wheels format)
//! - Directories
//!
//! # Prerequisites
//!
//! Download numpy from wasi-wheels:
//! ```bash
//! curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz \
//!     -o /tmp/numpy-wasi.tar.gz
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo run --example package_loading --features packages,native-extensions,embedded-stdlib --release
//! ```

use std::path::Path;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Package Loading Example ===\n");

    // Test with numpy tar.gz
    let numpy_tarball = Path::new("/tmp/numpy-wasi.tar.gz");

    if !numpy_tarball.exists() {
        eprintln!("numpy-wasi.tar.gz not found at /tmp/numpy-wasi.tar.gz");
        eprintln!();
        eprintln!("Download it with:");
        eprintln!(
            "  curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz -o /tmp/numpy-wasi.tar.gz"
        );
        return Ok(());
    }

    println!("--- Loading numpy from tar.gz ---\n");

    // No need to specify cache_dir - it's automatic!
    // Use embedded() for runtime+stdlib, late-linking overrides when package has .so files
    let start = Instant::now();
    let sandbox = eryx::Sandbox::embedded()
        .with_package(numpy_tarball)?
        .build()?;

    println!("  Sandbox created in {:?}", start.elapsed());

    // Test execution
    println!("\n--- Testing numpy ---\n");

    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
import numpy as np

# Basic array operations
a = np.array([1, 2, 3, 4, 5])
print(f"Array: {a}")
print(f"Sum: {a.sum()}")
print(f"Mean: {a.mean()}")

# Matrix operations
m = np.array([[1, 2], [3, 4]])
print(f"\nMatrix:\n{m}")
print(f"Determinant: {np.linalg.det(m):.1f}")

print("\nNumpy loaded via with_package()!")
"#,
        )
        .await?;

    println!("  Executed in {:?}", start.elapsed());
    println!("\nOutput:\n{}", result.stdout);

    // Test warm cache (automatic at $TMPDIR/eryx-cache)
    println!("--- Second sandbox (cache hit) ---\n");

    let start = Instant::now();
    let sandbox2 = eryx::Sandbox::embedded()
        .with_package(numpy_tarball)?
        .build()?;

    println!(
        "  Created in {:?} (cache hit for linked runtime)",
        start.elapsed()
    );

    let result = sandbox2
        .execute("import numpy; print(numpy.__version__)")
        .await?;
    println!("  numpy version: {}", result.stdout.trim());

    // Summary
    println!("\n=== Summary ===");
    println!("  with_package() auto-detects format (tar.gz, whl, directory)");
    println!("  Native extensions are auto-registered for late-linking");
    println!("  Caching is automatic at $TMPDIR/eryx-cache");

    Ok(())
}
