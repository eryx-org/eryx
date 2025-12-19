//! Example demonstrating WASM pre-compilation with optional Python pre-initialization.
//!
//! Pre-compiling the WASM component to native code AND pre-initializing Python
//! provides the fastest possible sandbox and session creation times:
//!
//! - Sandbox creation: ~234ns (with InstancePreCache)
//! - Session creation: ~1-5ms (vs ~450ms without pre-init)
//!
//! This example shows how to:
//!
//! 1. Pre-initialize Python (runs interpreter init, captures memory state) [with preinit]
//! 2. Pre-compile to native code
//! 3. Save for embedding in the binary
//!
//! Run with preinit (recommended for fastest sessions):
//!   `cargo run --example precompile --features preinit --release`
//!
//! Run without preinit (faster build, slower sessions):
//!   `cargo run --example precompile --release`

use std::time::Instant;

#[cfg(feature = "preinit")]
use std::path::Path;

#[cfg(feature = "embedded")]
use eryx::Session;

fn main() -> anyhow::Result<()> {
    let wasm_path = std::env::var("ERYX_WASM_PATH")
        .unwrap_or_else(|_| "crates/eryx-runtime/runtime.wasm".to_string());

    println!("=== Pre-compilation Example ===\n");
    println!("WASM path: {wasm_path}");

    // Read the WASM bytes
    let wasm_bytes = std::fs::read(&wasm_path)?;
    println!(
        "Original WASM size: {} bytes ({:.1} MB)",
        wasm_bytes.len(),
        wasm_bytes.len() as f64 / 1_000_000.0
    );

    // With preinit: do pre-initialization for faster session creation
    #[cfg(feature = "preinit")]
    let component_bytes = {
        let python_stdlib = find_python_stdlib()?;
        println!("Python stdlib: {}\n", python_stdlib.display());

        println!("--- Step 1: Pre-initializing Python ---");
        println!("This runs Python's interpreter initialization and captures the memory state.");
        let start = Instant::now();

        let rt = tokio::runtime::Runtime::new()?;
        let preinit_bytes = rt.block_on(async {
            eryx::preinit::pre_initialize(
                &python_stdlib,
                None, // No site-packages for base runtime
                &[],  // No imports for base runtime
                &[],  // No native extensions for base runtime
            )
            .await
        })?;

        let preinit_time = start.elapsed();
        println!("Pre-init time: {preinit_time:?}");
        println!(
            "Pre-initialized size: {} bytes ({:.1} MB)",
            preinit_bytes.len(),
            preinit_bytes.len() as f64 / 1_000_000.0
        );

        preinit_bytes
    };

    // Without preinit: skip pre-initialization
    #[cfg(not(feature = "preinit"))]
    let component_bytes = {
        println!("\n--- Step 1: Skipping pre-initialization ---");
        println!("Enable preinit feature for faster session creation.");
        println!("Without pre-init, session creation will take ~450ms instead of ~1-5ms.\n");
        wasm_bytes
    };

    // Step 2: Pre-compile to native code
    println!("\n--- Step 2: Pre-compiling to native code ---");
    let start = Instant::now();
    let precompiled = eryx::PythonExecutor::precompile(&component_bytes)?;
    let precompile_time = start.elapsed();
    println!("Pre-compile time: {precompile_time:?}");
    println!(
        "Pre-compiled size: {} bytes ({:.1} MB)",
        precompiled.len(),
        precompiled.len() as f64 / 1_000_000.0
    );

    // Step 3: Save to disk
    let cwasm_path = "crates/eryx-runtime/runtime.cwasm";
    println!("\n--- Step 3: Saving pre-compiled WASM ---");
    std::fs::write(cwasm_path, &precompiled)?;
    println!("Saved to: {cwasm_path}");

    // Step 4: Verify the freshly created cwasm works (requires embedded feature for unsafe APIs)
    #[cfg(feature = "embedded")]
    {
        println!("\n--- Step 4: Verification ---");
        println!("Loading freshly created {cwasm_path} to verify it works...");

        // Use embedded resources for stdlib (already extracted by EmbeddedResources)
        let resources = eryx::embedded::EmbeddedResources::get()?;

        let rt = tokio::runtime::Runtime::new()?;

        // Load the precompiled file we just created
        // SAFETY: We just created this file from PythonExecutor::precompile()
        #[allow(unsafe_code)]
        let sandbox = unsafe {
            eryx::Sandbox::builder()
                .with_precompiled_file(cwasm_path)
                .with_python_stdlib(resources.stdlib())
                .build()?
        };

        // Test sandbox creation (should be fast with precompiled)
        println!("\nSandbox creation (10x):");
        let start = Instant::now();
        for _ in 0..10 {
            #[allow(unsafe_code)]
            let _sandbox = unsafe {
                eryx::Sandbox::builder()
                    .with_precompiled_file(cwasm_path)
                    .with_python_stdlib(resources.stdlib())
                    .build()?
            };
        }
        let sandbox_time = start.elapsed();
        println!(
            "  10 sandboxes in {:?} (avg {:?})",
            sandbox_time,
            sandbox_time / 10
        );

        // Test session creation
        println!("\nSession creation (5x):");
        let start = Instant::now();
        for _ in 0..5 {
            let _session = rt.block_on(eryx::InProcessSession::new(&sandbox))?;
        }
        let session_time = start.elapsed();
        println!(
            "  5 sessions in {:?} (avg {:?})",
            session_time,
            session_time / 5
        );

        // Test execution in session
        println!("\nSession execution (10x):");
        let mut session = rt.block_on(eryx::InProcessSession::new(&sandbox))?;
        let start = Instant::now();
        for _ in 0..10 {
            rt.block_on(session.execute("x = 1 + 1"))?;
        }
        let exec_time = start.elapsed();
        println!(
            "  10 executions in {:?} (avg {:?})",
            exec_time,
            exec_time / 10
        );

        // Verify output works
        let result = rt.block_on(session.execute("print('Hello from pre-compiled sandbox!')"))?;
        println!("\nOutput: {}", result.stdout.trim());

        // Summary
        println!("\n=== Summary ===");
        #[cfg(feature = "preinit")]
        println!("Mode: Pre-initialized + Pre-compiled (fastest)");
        #[cfg(not(feature = "preinit"))]
        println!("Mode: Pre-compiled only (no pre-init)");
        println!();
        println!("Runtime performance:");
        println!("  Sandbox creation: {:?} avg", sandbox_time / 10);
        println!("  Session creation: {:?} avg", session_time / 5);
        println!("  Session execute:  {:?} avg", exec_time / 10);
    }

    #[cfg(not(feature = "embedded"))]
    {
        println!("\n--- Step 4: Verification skipped ---");
        println!("Run with --features embedded to verify the pre-compiled file works.");
    }

    println!();
    println!("Pre-compiled file saved to: {cwasm_path}");
    println!("This file is used by the `embedded` feature for fast sandbox creation.");

    Ok(())
}

/// Find the Python stdlib directory.
#[cfg(feature = "preinit")]
fn find_python_stdlib() -> anyhow::Result<std::path::PathBuf> {
    // Check env vars first (used in CI)
    for var in ["ERYX_PYTHON_STDLIB", "PYTHON_STDLIB_PATH"] {
        if let Ok(path) = std::env::var(var) {
            let p = Path::new(&path);
            if p.exists() && p.join("encodings").exists() {
                return Ok(p.to_path_buf());
            }
        }
    }

    // Check common locations
    let candidates = [
        // Relative to workspace root (when running from workspace)
        "crates/eryx-wasm-runtime/tests/python-stdlib",
        // Relative to example directory
        "../eryx-wasm-runtime/tests/python-stdlib",
    ];

    for candidate in &candidates {
        let path = Path::new(candidate);
        if path.exists() && path.join("encodings").exists() {
            return Ok(path.to_path_buf());
        }
    }

    anyhow::bail!(
        "Could not find Python stdlib. Tried: {:?}\n\
         Run `mise run setup-eryx-runtime-tests` to extract the stdlib, \
         or set ERYX_PYTHON_STDLIB environment variable.",
        candidates
    )
}
