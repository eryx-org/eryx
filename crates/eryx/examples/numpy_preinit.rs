// Examples use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

//! Example demonstrating pre-initialization with numpy.
//!
//! This example shows how to use pre-initialization to capture Python's
//! initialized state (including numpy) into the component, eliminating
//! startup overhead on subsequent sandbox creations.
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
//! cargo run --example numpy_preinit --features pre-init,precompiled --release
//! ```
//!
//! # Performance Impact
//!
//! - Without pre-init: ~50-100ms Python startup per execution
//! - With pre-init: ~1-2ms Python startup per execution

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

    // Get Python stdlib path
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let python_stdlib = std::path::PathBuf::from(&manifest_dir)
        .parent()
        .ok_or("Cannot find parent directory")?
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib");

    let site_packages = numpy_dir
        .parent()
        .ok_or("Cannot find site-packages directory")?;

    println!("=== Numpy Pre-Initialization Example ===\n");

    // Load extensions once
    println!("Loading numpy native extensions...");
    let start = Instant::now();
    let extensions = load_numpy_extensions(numpy_dir)?;
    println!(
        "  Loaded {} extensions in {:?}\n",
        extensions.len(),
        start.elapsed()
    );

    // Convert to NativeExtension structs
    let native_extensions: Vec<_> = extensions
        .iter()
        .map(|(name, bytes)| {
            eryx_runtime::linker::NativeExtension::new(name.clone(), bytes.clone())
        })
        .collect();

    // Step 1: Pre-initialize the component (links + runs Python init + imports numpy)
    // This internally creates both the original component (with real WASI) and a
    // stubbed component (with trap-on-call WASI) to prevent file handles from being
    // captured in the memory snapshot.
    println!("Step 1: Pre-initializing component (linking + importing numpy)...");
    let start = Instant::now();

    let preinit_component = eryx::preinit::pre_initialize(
        &python_stdlib,
        Some(site_packages),
        &["numpy"], // Pre-import numpy during init
        &native_extensions,
    )
    .await?;

    println!(
        "  Pre-initialized in {:?} ({:.1} MB)",
        start.elapsed(),
        preinit_component.len() as f64 / 1_000_000.0
    );

    // Step 2: Pre-compile the pre-initialized component
    println!("\nStep 2: Pre-compiling component...");
    let start = Instant::now();

    let precompiled = eryx::PythonExecutor::precompile(&preinit_component)?;
    println!(
        "  Pre-compiled in {:?} ({:.1} MB)",
        start.elapsed(),
        precompiled.len() as f64 / 1_000_000.0
    );

    // Step 4: Cache the pre-compiled component
    let cache_dir = Path::new("/tmp/eryx-preinit-cache");
    let _ = std::fs::remove_dir_all(cache_dir);
    std::fs::create_dir_all(cache_dir)?;
    let cache_file = cache_dir.join("numpy-preinit.cwasm");
    std::fs::write(&cache_file, &precompiled)?;
    println!(
        "\nCached pre-initialized component at: {}",
        cache_file.display()
    );

    // Step 5: Create sandboxes using the pre-initialized, pre-compiled component
    println!("\n=== Creating Sandboxes ===\n");

    // First sandbox (load from cache)
    println!("Creating sandbox from pre-initialized cache...");
    let start = Instant::now();
    // SAFETY: We just created this precompiled file ourselves from known-good input
    let sandbox = unsafe {
        Sandbox::builder()
            .with_precompiled_bytes(precompiled.clone())
            .with_python_stdlib(&python_stdlib)
            .with_site_packages(site_packages)
            .build()?
    };
    println!("  Created in {:?}", start.elapsed());

    // Test numpy is already imported and ready
    println!("\nTesting numpy...");
    let start = Instant::now();
    let result = sandbox
        .execute(
            r#"
import numpy as np
a = np.array([1, 2, 3, 4, 5])
print(f"Array: {a}")
print(f"Sum: {a.sum()}")
print(f"Mean: {a.mean()}")
"#,
        )
        .await?;
    println!("  Executed in {:?}", start.elapsed());
    println!("Output:\n{}", result.stdout);

    // Create multiple sandboxes to show the speed
    println!("=== Multiple Sandbox Creation Benchmark ===\n");

    let mut times = vec![];
    for i in 0..5 {
        let start = Instant::now();
        // SAFETY: We created this precompiled data ourselves
        let sandbox = unsafe {
            Sandbox::builder()
                .with_precompiled_bytes(precompiled.clone())
                .with_python_stdlib(&python_stdlib)
                .with_site_packages(site_packages)
                .build()?
        };
        let elapsed = start.elapsed();
        times.push(elapsed);

        // Quick test
        let result = sandbox
            .execute("import numpy as np; print(np.sum([1,2,3]))")
            .await?;
        assert!(result.stdout.contains("6"));

        println!("  Sandbox {}: {:?}", i + 1, elapsed);
    }

    let avg = times.iter().map(|t| t.as_millis()).sum::<u128>() / times.len() as u128;
    println!("\n  Average sandbox creation: {}ms", avg);

    println!("\n=== Pre-Initialization Complete ===");
    println!("\nThe pre-initialized, pre-compiled component can be cached and reused");
    println!("for fast sandbox creation (~10ms) with numpy already initialized.");

    Ok(())
}
