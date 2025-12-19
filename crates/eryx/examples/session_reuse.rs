//! Session Reuse Example
//!
//! This example demonstrates session-based execution for
//! maintaining state between Python executions:
//!
//! 1. **SessionExecutor** - Core executor that keeps WASM instance alive
//! 2. **InProcessSession** - High-level session API wrapping SessionExecutor
//! 3. **State Snapshots** - Capture and restore Python state
//!
//! Run with: `cargo run --example session_reuse`
//!
//! Note: Requires the runtime.wasm to be built first.
//! See crates/eryx-runtime/README.md for instructions.

use std::sync::Arc;
use std::time::Instant;

use eryx::{
    InProcessSession, PythonExecutor, PythonStateSnapshot, Sandbox, Session, SessionExecutor,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Session Reuse Examples ===\n");

    // Build the sandbox once using embedded runtime - all sessions will share it
    let sandbox = Sandbox::embedded().build()?;

    // Also create a PythonExecutor for SessionExecutor demo
    let executor = Arc::new(PythonExecutor::from_embedded_runtime()?);

    // Run each demo
    demo_session_executor(&executor).await?;
    println!();
    demo_in_process_session(&sandbox).await?;
    println!();
    demo_state_snapshots(&executor).await?;
    println!();
    benchmark_comparison(&sandbox, &executor).await?;

    Ok(())
}

/// SessionExecutor Demo
///
/// The core building block for session reuse. Keeps the WASM instance
/// alive between executions, avoiding Python initialization overhead.
async fn demo_session_executor(executor: &Arc<PythonExecutor>) -> anyhow::Result<()> {
    println!("--- SessionExecutor (Core) ---");
    println!("Keeps WASM Store and Instance alive between executions.");
    println!("This is the foundation for all session types.\n");

    // Create a session executor with no callbacks
    let mut session = SessionExecutor::new(executor.clone(), &[]).await?;

    // Execute multiple statements, building up state
    let start = Instant::now();

    let output = session
        .execute("x = 1", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "Execute 'x = 1': '{}' (execution #{})",
        output.stdout,
        session.execution_count()
    );

    let output = session
        .execute("y = 2", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "Execute 'y = 2': '{}' (execution #{})",
        output.stdout,
        session.execution_count()
    );

    // This should see variables from previous executions once
    // runtime.py is modified to persist exec_globals
    let output = session
        .execute("print(x + y)", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "Execute 'print(x + y)': '{}' (execution #{})",
        output.stdout,
        session.execution_count()
    );

    let total = start.elapsed();
    println!("\nTotal time for 3 executions: {:?}", total);
    println!("Average per execution: {:?}", total / 3);

    // Reset and verify state is cleared
    session.reset(&[]).await?;
    println!(
        "\nAfter reset, execution count: {}",
        session.execution_count()
    );

    Ok(())
}

/// In-Process Session Demo
///
/// High-level API that wraps SessionExecutor.
/// State persists between executions - variables, functions, classes are available
/// across calls.
async fn demo_in_process_session(sandbox: &Sandbox) -> anyhow::Result<()> {
    println!("--- InProcessSession (High-Level API) ---");
    println!("Wraps SessionExecutor for easy state persistence.\n");

    let mut session = InProcessSession::new(sandbox).await?;

    // Execute multiple statements - state persists between calls!
    let start = Instant::now();

    let result = session.execute("x = 1").await?;
    println!(
        "Execute 'x = 1': '{}' ({:?})",
        result.stdout, result.stats.duration
    );

    let result = session.execute("y = 2").await?;
    println!(
        "Execute 'y = 2': '{}' ({:?})",
        result.stdout, result.stats.duration
    );

    // This works because x and y persist from previous calls!
    let result = session.execute("print(x + y)").await?;
    println!(
        "Execute 'print(x + y)': '{}' ({:?})",
        result.stdout, result.stats.duration
    );

    let total = start.elapsed();
    println!("\nTotal time for 3 executions: {:?}", total);
    println!("Execution count: {}", session.execution_count());

    // Reset clears all state
    session.reset().await?;
    println!("\nAfter reset, session state is cleared.");

    Ok(())
}

/// State Snapshots Demo
///
/// Demonstrates capturing and restoring Python state.
async fn demo_state_snapshots(executor: &Arc<PythonExecutor>) -> anyhow::Result<()> {
    println!("--- State Snapshots ---");
    println!("Capture and restore Python state using pickle serialization.");
    println!("Enables state persistence across process restarts.\n");

    let mut session = SessionExecutor::new(executor.clone(), &[]).await?;

    // Build up some state
    session
        .execute("x = 10", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    session
        .execute("y = 20", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    session
        .execute("data = [1, 2, 3, 4, 5]", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    println!("Created state: x=10, y=20, data=[1,2,3,4,5]");

    // Take a snapshot
    let snapshot_start = Instant::now();
    let snapshot = session.snapshot_state().await?;
    let snapshot_duration = snapshot_start.elapsed();

    println!("\nSnapshot captured:");
    println!("  - Size: {} bytes", snapshot.size());
    println!("  - Capture time: {:?}", snapshot_duration);
    println!("  - Timestamp: {}", snapshot.metadata().timestamp_ms);

    // Serialize to bytes (what you'd save to disk/database)
    let bytes = snapshot.to_bytes();
    println!("  - Serialized size: {} bytes", bytes.len());

    // Modify the state
    session
        .execute("x = 999", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let output = session
        .execute("print(f'x after modification: {x}')", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("\n{}", output.stdout);

    // Restore from snapshot
    let restore_start = Instant::now();
    session.restore_state(&snapshot).await?;
    let restore_duration = restore_start.elapsed();

    let output = session
        .execute("print(f'x after restore: {x}')", &[], None, None)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("{}", output.stdout);
    println!("Restore time: {:?}", restore_duration);

    // Demonstrate deserializing from bytes (simulating load from storage)
    println!("\nSimulating load from storage...");
    let restored_snapshot = PythonStateSnapshot::from_bytes(&bytes)?;
    println!(
        "Deserialized snapshot: {} bytes, timestamp {}",
        restored_snapshot.size(),
        restored_snapshot.metadata().timestamp_ms
    );

    // Clear state and restore from deserialized snapshot
    session.clear_state().await?;
    println!("State cleared");

    session.restore_state(&restored_snapshot).await?;
    let output = session
        .execute(
            "print(f'After clear+restore: x={x}, y={y}, data={data}')",
            &[],
            None,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("{}", output.stdout);

    Ok(())
}

/// Compare execution times between regular sandbox and sessions.
async fn benchmark_comparison(
    sandbox: &Sandbox,
    executor: &Arc<PythonExecutor>,
) -> anyhow::Result<()> {
    println!("--- Performance Comparison ---\n");

    const ITERATIONS: usize = 5;

    // Regular sandbox (fresh instance each time)
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        sandbox.execute("x = 1").await?;
    }
    let regular_time = start.elapsed();

    // SessionExecutor (reused instance - core approach)
    let mut session_executor = SessionExecutor::new(executor.clone(), &[]).await?;
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        session_executor
            .execute("x = 1", &[], None, None)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    let executor_time = start.elapsed();

    // In-process session (high-level API)
    let mut session = InProcessSession::new(sandbox).await?;
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        session.execute("x = 1").await?;
    }
    let session_time = start.elapsed();

    println!(
        "Regular sandbox ({} executions): {:?}",
        ITERATIONS, regular_time
    );
    println!(
        "SessionExecutor ({} executions): {:?}",
        ITERATIONS, executor_time
    );
    println!(
        "InProcessSession ({} executions): {:?}",
        ITERATIONS, session_time
    );

    if executor_time.as_nanos() > 0 {
        println!(
            "\nSessionExecutor speedup vs regular: {:.2}x",
            regular_time.as_secs_f64() / executor_time.as_secs_f64()
        );
    }

    println!("\nNote: SessionExecutor persists state between executions.");
    println!("Use snapshot_state()/restore_state() for serialization.");
    println!("See plans/SANDBOX_REUSE.md for implementation details.");

    Ok(())
}
