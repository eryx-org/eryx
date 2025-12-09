//! Example demonstrating the embedded runtime feature for fast sandbox creation.
//!
//! The `embedded-runtime` feature pre-compiles the Python runtime WASM at build time,
//! providing ~50x faster sandbox creation with a safe API (no `unsafe` blocks needed!).
//!
//! Run with: `cargo run --example embedded_runtime --features embedded-runtime --release`
//!
//! Compare with the normal WASM loading:
//!   `cargo run --example precompile --release`

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    println!("=== Embedded Runtime Example ===\n");

    // Step 1: Create sandbox using embedded runtime (fast!)
    println!("--- Creating sandbox with embedded runtime ---");
    let start = Instant::now();
    let sandbox = eryx::Sandbox::builder().with_embedded_runtime().build()?;
    let load_time = start.elapsed();
    println!("Sandbox creation time: {load_time:?}");

    // Step 2: Multiple creations to show consistent performance
    println!("\n--- Multiple sandbox creations (10x) ---");
    let start = Instant::now();
    for _ in 0..10 {
        let _sandbox = eryx::Sandbox::builder().with_embedded_runtime().build()?;
    }
    let total_time = start.elapsed();
    let avg_time = total_time / 10;
    println!("Total time for 10 creations: {total_time:?}");
    println!("Average per creation: {avg_time:?}");

    // Step 3: Verify execution works
    println!("\n--- Verify execution works ---");
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        sandbox
            .execute("print('Hello from embedded runtime sandbox!')")
            .await
    })?;
    println!("Output: {}", result.stdout);

    // Step 4: Show per-execution overhead
    println!("\n--- Per-execution overhead (10x) ---");
    let start = Instant::now();
    for _ in 0..10 {
        rt.block_on(async { sandbox.execute("pass").await })?;
    }
    let exec_total = start.elapsed();
    let exec_avg = exec_total / 10;
    println!("Total time for 10 executions: {exec_total:?}");
    println!("Average per execution: {exec_avg:?}");

    // Summary
    println!("\n=== Summary ===");
    println!("Sandbox Creation:");
    println!("  With embedded runtime: {avg_time:?} (average)");
    println!("  Compare to ~500-600ms without pre-compilation!");
    println!("\nPer-Execution:");
    println!("  Average overhead: {exec_avg:?}");
    println!("\nBenefits of embedded-runtime feature:");
    println!("  ✓ ~50x faster sandbox creation");
    println!("  ✓ Safe API (no unsafe blocks needed)");
    println!("  ✓ Zero configuration required");
    println!("  ✓ Pre-compiled at build time");

    Ok(())
}
