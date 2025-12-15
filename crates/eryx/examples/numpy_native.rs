//! Example demonstrating native Python extension support with numpy.
//!
//! This example shows how to use late-linking to add numpy's native extensions
//! to the sandbox at creation time.
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
//! cargo run --example numpy_native --features native-extensions
//! ```

use std::path::Path;

use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Path to extracted numpy from wasi-wheels
    let numpy_dir = Path::new("/tmp/numpy");

    if !numpy_dir.exists() {
        eprintln!("numpy not found at /tmp/numpy");
        eprintln!();
        eprintln!("Download it with:");
        eprintln!("  curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz -o /tmp/numpy-wasi.tar.gz");
        eprintln!("  tar -xzf /tmp/numpy-wasi.tar.gz -C /tmp/");
        return Ok(());
    }

    // Get Python stdlib path (needed for core Python modules like encodings)
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let python_stdlib = std::path::PathBuf::from(&manifest_dir)
        .parent()
        .ok_or("Cannot find parent directory")?
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib");

    println!("Loading numpy native extensions...");

    // Find all .so files in the numpy directory
    let mut builder = Sandbox::builder();
    let mut extension_count = 0;

    for entry in walkdir::WalkDir::new(numpy_dir) {
        let entry = entry?;
        let path = entry.path();

        if let Some(ext) = path.extension()
            && ext == "so"
        {
            // Python will dlopen with the full path from the mounted filesystem.
            // Since we mount /tmp at /site-packages, the path will be
            // /site-packages/numpy/core/foo.so
            let numpy_parent = numpy_dir
                .parent()
                .ok_or("Cannot find numpy parent directory")?;
            let relative_path = path.strip_prefix(numpy_parent)?;
            let dlopen_path = format!("/site-packages/{}", relative_path.to_string_lossy());
            let bytes = std::fs::read(path)?;

            println!("  Adding: {} ({} bytes)", dlopen_path, bytes.len());
            builder = builder.with_native_extension(dlopen_path, bytes);
            extension_count += 1;
        }
    }

    println!("Added {} native extensions", extension_count);

    // Mount Python stdlib and numpy's Python files via site-packages
    let site_packages = numpy_dir
        .parent()
        .ok_or("Cannot find site-packages directory")?;
    builder = builder
        .with_python_stdlib(&python_stdlib)
        .with_site_packages(site_packages);

    println!("Building sandbox with late-linked numpy...");
    let sandbox = builder.build()?;

    // Test basic numpy operations
    println!("\n--- Running numpy tests ---\n");

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

    let result = sandbox.execute(code).await?;

    println!("{}", result.stdout);

    if result.stdout.contains("Numpy is working!") {
        println!("\n--- SUCCESS: numpy native extensions work! ---");
    } else {
        eprintln!("\n--- FAILED: numpy test did not complete ---");
    }

    Ok(())
}
