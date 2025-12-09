//! Example demonstrating WASM pre-compilation for faster sandbox creation.
//!
//! Pre-compiling the WASM component to native code provides ~40x faster
//! sandbox creation times. This example shows how to:
//!
//! 1. Pre-compile WASM to native code (safe operation)
//! 2. Load from pre-compiled bytes (unsafe - requires trust)
//! 3. Measure the performance difference
//!
//! Run with: `cargo run --example precompile --release`

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let wasm_path = std::env::var("ERYX_WASM_PATH")
        .unwrap_or_else(|_| "crates/eryx-runtime/runtime.wasm".to_string());

    println!("=== Pre-compilation Example ===\n");
    println!("WASM path: {wasm_path}\n");

    // Step 1: Normal loading (includes compilation) - for comparison
    println!("--- Step 1: Loading from WASM (includes compilation) ---");
    let start = Instant::now();
    let sandbox = eryx::Sandbox::builder()
        .with_wasm_file(&wasm_path)
        .build()?;
    let normal_load_time = start.elapsed();
    println!("Time: {normal_load_time:?}");
    drop(sandbox);

    // Step 2: Pre-compile to bytes (safe operation)
    println!("\n--- Step 2: Pre-compiling WASM (safe) ---");
    let start = Instant::now();
    let precompiled = eryx::PythonExecutor::precompile_file(&wasm_path)?;
    let precompile_time = start.elapsed();
    println!("Pre-compile time: {precompile_time:?}");
    println!(
        "Pre-compiled size: {} bytes ({:.1} MB)",
        precompiled.len(),
        precompiled.len() as f64 / 1_000_000.0
    );

    // Step 3: Load from pre-compiled bytes (unsafe - requires trust)
    println!("\n--- Step 3: Loading from pre-compiled bytes (unsafe) ---");
    let start = Instant::now();
    // SAFETY: We just created these precompiled bytes ourselves using
    // PythonExecutor::precompile_file, so we trust them.
    #[allow(unsafe_code)]
    let sandbox = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_bytes(precompiled.clone())
            .build()?
    };
    let precompiled_load_time = start.elapsed();
    println!("Time: {precompiled_load_time:?}");
    drop(sandbox);

    // Step 4: Multiple loads to show consistent performance
    println!("\n--- Step 4: Multiple loads from pre-compiled (10x) ---");
    let start = Instant::now();
    for _ in 0..10 {
        // SAFETY: Same precompiled bytes we created above
        #[allow(unsafe_code)]
        let sandbox = unsafe {
            eryx::Sandbox::builder()
                .with_precompiled_bytes(precompiled.clone())
                .build()?
        };
        drop(sandbox);
    }
    let total_time = start.elapsed();
    let avg_time = total_time / 10;
    println!("Total time for 10 loads: {total_time:?}");
    println!("Average per load: {avg_time:?}");

    // Step 5: Verify execution still works
    println!("\n--- Step 5: Verify execution works ---");
    // SAFETY: Same precompiled bytes we created above
    #[allow(unsafe_code)]
    let sandbox = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_bytes(precompiled.clone())
            .build()?
    };
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        sandbox
            .execute("print('Hello from pre-compiled sandbox!')")
            .await
    })?;
    println!("Output: {}", result.stdout);

    // Step 6: Show per-execution overhead
    println!("\n--- Step 6: Per-execution overhead ---");
    let start = Instant::now();
    for _ in 0..10 {
        rt.block_on(async { sandbox.execute("pass").await })?;
    }
    let exec_total = start.elapsed();
    let exec_avg = exec_total / 10;
    println!("Average per execution: {exec_avg:?}");

    // Summary
    println!("\n=== Summary ===");
    println!("Sandbox Creation:");
    println!("  Normal WASM load:      {normal_load_time:?}");
    println!("  Pre-compiled load:     {precompiled_load_time:?}");
    println!(
        "  Speedup:               {:.1}x faster",
        normal_load_time.as_secs_f64() / precompiled_load_time.as_secs_f64()
    );
    println!("\nPer-Execution:");
    println!("  Average overhead:      {exec_avg:?}");
    println!("\nPre-compile once, load many times:");
    println!("  Pre-compile cost:      {precompile_time:?}");
    println!("  Per-load cost:         {avg_time:?}");

    // Save pre-compiled WASM to disk for faster test runs
    let cwasm_path = "crates/eryx-runtime/runtime.cwasm";
    println!("\n=== Saving Pre-compiled WASM ===");
    println!("Saving to: {cwasm_path}");
    std::fs::write(cwasm_path, &precompiled)?;
    println!(
        "Saved {} bytes ({:.1} MB)",
        precompiled.len(),
        precompiled.len() as f64 / 1_000_000.0
    );
    println!("\nTo run tests with precompiled WASM:");
    println!("  cargo nextest run --test session_state_persistence --features precompiled");

    Ok(())
}
