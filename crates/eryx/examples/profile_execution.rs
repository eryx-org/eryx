//! Profiling harness for session execution overhead.
//!
//! Run with samply:
//!   cargo build --example profile_execution --features embedded --release
//!   samply record ./target/release/examples/profile_execution
//!
//! Or with a specific iteration count:
//!   samply record ./target/release/examples/profile_execution 5000

use std::time::Instant;

use eryx::Sandbox;
use eryx::Session;
use eryx::session::InProcessSession;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20000);

    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        eprintln!("Creating sandbox...");
        let sandbox = Sandbox::embedded().build()?;

        eprintln!("Creating session...");
        let mut session = InProcessSession::new(&sandbox).await?;

        // Warm up
        eprintln!("Warming up (10 iterations)...");
        for _ in 0..10 {
            session.execute("x = 1").await?;
        }

        // Profile loop
        eprintln!("Profiling {iterations} iterations...");
        let start = Instant::now();

        for _ in 0..iterations {
            // Simple assignment - minimal Python work
            session.execute("x = 1").await?;
        }

        let elapsed = start.elapsed();
        eprintln!("\nResults:");
        eprintln!("  Total time: {elapsed:?}");
        eprintln!("  Iterations: {iterations}");
        eprintln!("  Average: {:?} per execution", elapsed / iterations);
        eprintln!(
            "  Throughput: {:.0} executions/sec",
            iterations as f64 / elapsed.as_secs_f64()
        );

        Ok::<_, Box<dyn std::error::Error>>(())
    })?;

    Ok(())
}
