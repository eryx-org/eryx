//! Profiling harness for stateless (fresh-instance) execution overhead.
//!
//! Each iteration goes through the full `Sandbox::execute()` path: a new
//! `Store`, `instantiate_async` from the cached `InstancePre`, one `execute`
//! export call, and teardown. This is the path a `SandboxFactory` render
//! takes, so it is the one to profile for per-request latency.
//!
//! Run with samply:
//!   cargo build --example profile_stateless --features embedded --release
//!   samply record ./target/release/examples/profile_stateless
//!
//! Or count page faults per execution:
//!   perf stat -e page-faults ./target/release/examples/profile_stateless 1000
//!
//! Arguments: `[iterations] [code] [gap_us]`. `gap_us` sleeps between
//! executions so background work (warm-instance replenishment) gets the same
//! chance it has between real requests; only the executions are timed.

use std::time::{Duration, Instant};

use eryx::Sandbox;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let iterations: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let code = args.next().unwrap_or_else(|| "pass".to_string());
    let gap = Duration::from_micros(args.next().and_then(|s| s.parse().ok()).unwrap_or(0));

    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        eprintln!("Creating sandbox...");
        // Trace collection (sys.settrace) is on by default; `ERYX_PROFILE_TRACE=0`
        // turns it off to measure without per-event trace overhead.
        let collect_trace = !std::env::var("ERYX_PROFILE_TRACE").is_ok_and(|v| v.trim() == "0");
        let sandbox = Sandbox::embedded()
            .with_trace_collection(collect_trace)
            .build()?;
        let executor = sandbox.executor();

        eprintln!("Warming up (10 iterations)...");
        for _ in 0..10 {
            sandbox.execute(&code).await?;
            std::thread::sleep(gap);
        }

        eprintln!("Profiling {iterations} iterations of {code:?} with a {gap:?} gap...");
        let mut executing = Duration::ZERO;
        let mut warm_hits = 0u32;

        for _ in 0..iterations {
            if executor.warm_instances_ready() > 0 {
                warm_hits += 1;
            }
            let start = Instant::now();
            sandbox.execute(&code).await?;
            executing += start.elapsed();
            std::thread::sleep(gap);
        }

        eprintln!("\nResults:");
        eprintln!("  Time executing: {executing:?}");
        eprintln!("  Iterations: {iterations}");
        eprintln!("  Warm instance available: {warm_hits}");
        eprintln!("  Average: {:?} per execution", executing / iterations);
        eprintln!(
            "  Throughput: {:.0} executions/sec",
            iterations as f64 / executing.as_secs_f64()
        );

        Ok::<_, Box<dyn std::error::Error>>(())
    })?;

    Ok(())
}
