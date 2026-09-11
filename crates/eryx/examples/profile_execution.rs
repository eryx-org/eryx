//! Profiling harness for session execution overhead.
//!
//! Run with samply:
//!   cargo build --example profile_execution --features embedded --release
//!   samply record ./target/release/examples/profile_execution
//!
//! Or with a specific iteration count:
//!   samply record ./target/release/examples/profile_execution 5000
//!
//! Set `ERYX_PROFILE_TRACE=0` to disable trace collection (`sys.settrace`),
//! which is on by default and dominates anything heavier than `pass`.

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
        // Trace collection (sys.settrace) is on by default; `ERYX_PROFILE_TRACE=0`
        // turns it off to measure without per-event trace overhead.
        let collect_trace = !std::env::var("ERYX_PROFILE_TRACE").is_ok_and(|v| v.trim() == "0");
        eprintln!("  trace collection: {collect_trace}");
        let sandbox = Sandbox::embedded()
            .with_trace_collection(collect_trace)
            .build()?;

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
