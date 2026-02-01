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
        matches!(error, eryx::Error::FuelExhausted { .. }),
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
    assert!(result.unwrap().stdout.contains("recovered"));
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
