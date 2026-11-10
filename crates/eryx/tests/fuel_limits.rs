//! Integration tests for fuel-based instruction limiting.
//!
//! These tests verify that fuel limits work correctly for bounding
//! execution at the instruction level.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eryx::{PythonExecutor, SessionExecutor};

/// Shared executor to avoid repeated WASM loading across tests.
static SHARED_EXECUTOR: OnceLock<Arc<PythonExecutor>> = OnceLock::new();

fn get_shared_executor() -> Arc<PythonExecutor> {
    SHARED_EXECUTOR
        .get_or_init(|| Arc::new(create_executor()))
        .clone()
}

/// Create a PythonExecutor, using embedded resources if available.
fn create_executor() -> PythonExecutor {
    #[cfg(feature = "embedded")]
    {
        let resources =
            eryx::embedded::EmbeddedResources::get().expect("Failed to extract embedded resources");

        #[allow(unsafe_code)]
        unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
            .expect("Failed to load embedded runtime")
            .with_python_stdlib(resources.stdlib())
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        let path = runtime_wasm_path();
        PythonExecutor::from_file(&path)
            .unwrap_or_else(|e| panic!("Failed to load runtime.wasm from {:?}: {}", path, e))
            .with_python_stdlib(&stdlib_path)
    }
}

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> PathBuf {
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

async fn create_session() -> SessionExecutor {
    let executor = get_shared_executor();
    SessionExecutor::new(executor, &[])
        .await
        .expect("Failed to create session")
}

// =============================================================================
// Fuel Tracking Tests
// =============================================================================

#[tokio::test]
async fn test_fuel_consumed_is_tracked() {
    let mut session = create_session().await;

    let result = session
        .execute("x = 1 + 1")
        .run()
        .await
        .expect("Simple execution should succeed");

    // Fuel should always be tracked
    assert!(
        result.fuel_consumed.is_some(),
        "fuel_consumed should be present"
    );
    let fuel = result.fuel_consumed.unwrap();
    assert!(fuel > 0, "Some fuel should be consumed: {}", fuel);
}

#[tokio::test]
async fn test_more_work_consumes_more_fuel() {
    let mut session = create_session().await;

    // Simple expression
    let result1 = session
        .execute("x = 1")
        .run()
        .await
        .expect("Should succeed");
    let fuel1 = result1.fuel_consumed.unwrap();

    // Reset to get fresh state
    session.reset(&[]).await.expect("Reset should work");

    // More complex computation
    let result2 = session
        .execute("x = sum(range(1000))")
        .run()
        .await
        .expect("Should succeed");
    let fuel2 = result2.fuel_consumed.unwrap();

    assert!(
        fuel2 > fuel1,
        "More computation should consume more fuel: {} vs {}",
        fuel2,
        fuel1
    );
}

// =============================================================================
// Fuel Limit Tests
// =============================================================================

#[tokio::test]
async fn test_fuel_limit_exceeded_produces_error() {
    let mut session = create_session().await;

    // Set a fuel limit that will be exceeded by a large loop.
    // Note: Even simple Python code needs millions of WASM instructions,
    // so we need a limit large enough to run some code but small enough
    // that a big loop will exceed it.
    let result = session
        .execute("for i in range(1000000): pass")
        .with_fuel_limit(100_000_000) // 100M instructions
        .run()
        .await;

    assert!(result.is_err(), "Should fail when fuel is exhausted");
    let error = result.unwrap_err();
    assert!(
        matches!(error.kind(), eryx::ErrorKind::FuelExhausted { .. }),
        "Error should be FuelExhausted variant: {:?}",
        error
    );
}

#[tokio::test]
async fn test_fuel_limit_allows_code_within_limit() {
    let mut session = create_session().await;

    // Set a generous fuel limit - Python needs many WASM instructions
    // even for simple operations
    let result = session
        .execute("x = 1 + 1")
        .with_fuel_limit(500_000_000) // 500M instructions should be plenty
        .run()
        .await;

    assert!(
        result.is_ok(),
        "Simple code should succeed within limit: {:?}",
        result
    );
}

#[tokio::test]
async fn test_session_fuel_limit_persists() {
    let mut session = create_session().await;

    // Set fuel limit on session - generous enough for simple operations
    session.set_fuel_limit(Some(500_000_000));

    // Multiple executions should all respect the limit
    for i in 0..3 {
        let result = session.execute(format!("x = {}", i)).run().await;
        assert!(result.is_ok(), "Execution {} should succeed", i);
    }
}

#[tokio::test]
async fn test_per_execution_fuel_limit_overrides_session() {
    let mut session = create_session().await;

    // Set a generous session limit
    session.set_fuel_limit(Some(1_000_000_000));

    // But use a restrictive per-execution limit that a loop will exceed
    let result = session
        .execute("for i in range(1000000): pass")
        .with_fuel_limit(100_000_000) // Override with tighter limit
        .run()
        .await;

    assert!(result.is_err(), "Per-execution limit should be respected");
}

// =============================================================================
// Recovery Tests
// =============================================================================

#[tokio::test]
async fn test_session_recovers_after_fuel_exhaustion() {
    let mut session = create_session().await;

    // First execution exhausts fuel
    let result = session
        .execute("for i in range(1000000): pass")
        .with_fuel_limit(100_000_000)
        .run()
        .await;
    assert!(result.is_err(), "Should fail from fuel exhaustion");

    // Reset the session
    session.reset(&[]).await.expect("Reset should work");

    // Session should work again with fresh fuel (no limit set)
    let result = session.execute("print('recovered')").run().await;
    assert!(
        result.is_ok(),
        "Session should recover after reset: {:?}",
        result
    );
    assert!(result.unwrap().stdout_text().contains("recovered"));
}

// =============================================================================
// Determinism Tests
// =============================================================================

#[tokio::test]
async fn test_fuel_consumption_is_deterministic() {
    let executor = get_shared_executor();
    let code = "x = sum(range(100))";

    // Run the same code multiple times and check fuel is consistent
    let mut fuel_values = Vec::new();
    for _ in 0..3 {
        let mut session = SessionExecutor::new(executor.clone(), &[])
            .await
            .expect("Failed to create session");

        let result = session.execute(code).run().await.expect("Should succeed");
        fuel_values.push(result.fuel_consumed.unwrap());
    }

    // All runs should consume the same amount of fuel
    assert!(
        fuel_values.iter().all(|&f| f == fuel_values[0]),
        "Fuel consumption should be deterministic: {:?}",
        fuel_values
    );
}

// =============================================================================
// Failed Runs Report Fuel
// =============================================================================

#[tokio::test]
async fn test_fuel_is_reported_when_execution_times_out() {
    let mut session = create_session().await;
    session.set_execution_timeout(Some(std::time::Duration::from_millis(500)));

    let error = session
        .execute("while True: pass")
        .run()
        .await
        .expect_err("an infinite loop times out");

    assert!(
        matches!(error.kind(), eryx::ErrorKind::Timeout(_)),
        "{error:?}"
    );
    let fuel = error.stats().and_then(|stats| stats.fuel_consumed);
    assert!(
        fuel.is_some_and(|fuel| fuel > 100_000_000),
        "a run that spun until its timeout has used fuel: {fuel:?}"
    );
}

#[tokio::test]
async fn test_fuel_is_reported_when_script_raises() {
    let mut session = create_session().await;
    let cheap = session
        .execute("raise ValueError('early')")
        .run()
        .await
        .expect_err("raises");
    let heavy = session
        .execute("sum(range(200_000))\nraise ValueError('late')")
        .run()
        .await
        .expect_err("raises");

    let fuel = |error: &eryx::Error| error.stats().and_then(|stats| stats.fuel_consumed);
    assert!(matches!(
        heavy.kind(),
        eryx::ErrorKind::PythonException { .. }
    ));
    assert!(
        fuel(&heavy) > fuel(&cheap) && fuel(&cheap).is_some(),
        "heavy work before raising must show up in the fuel: {:?} vs {:?}",
        fuel(&heavy),
        fuel(&cheap)
    );
}

#[tokio::test]
async fn test_fuel_exhaustion_stats_agree_with_the_variant() {
    let mut session = create_session().await;

    let error = session
        .execute("while True: pass")
        .with_fuel_limit(5_000_000)
        .run()
        .await
        .expect_err("runs out of fuel");

    let eryx::ErrorKind::FuelExhausted { consumed, .. } = error.kind() else {
        panic!("expected FuelExhausted, got {error:?}");
    };
    assert_eq!(
        error.stats().and_then(|stats| stats.fuel_consumed),
        Some(*consumed)
    );
}

#[tokio::test]
async fn test_no_stats_when_the_guest_never_ran() {
    let mut session = create_session().await;

    let error = session
        .execute("x = 1\0")
        .run()
        .await
        .expect_err("NUL bytes are rejected");

    assert!(error.stats().is_none(), "{error:?}");
}
