//! Integration tests for execution cancellation.
//!
//! These tests verify that execute_cancellable and ExecutionHandle work correctly,
//! including cancellation, waiting for results, and error handling.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::time::Duration;

use eryx::{Error, ResourceLimits, Sandbox};

// =============================================================================
// Helper Functions
// =============================================================================

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
    // Check ERYX_PYTHON_STDLIB env var first (used in CI)
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    // Fall back to relative path from crate directory
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

/// Create a sandbox builder with the appropriate WASM source.
fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    // When embedded feature is available, use it (more reliable)
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }

    // Fallback to explicit paths for testing without embedded feature
    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

/// Create a sandbox builder with a short execution timeout for cancellation tests.
/// This ensures tests don't hang for 30s if cancellation fails.
fn sandbox_builder_with_short_timeout() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has>
{
    sandbox_builder().with_resource_limits(ResourceLimits {
        execution_timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
}

// =============================================================================
// Basic Cancellation Tests
// =============================================================================

#[tokio::test]
async fn test_execute_cancellable_completes_normally() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("print('Hello from cancellable!')");

    let result = handle.wait().await;
    assert!(result.is_ok(), "Should complete normally: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Hello from cancellable!"));
}

#[tokio::test]
async fn test_execute_cancellable_cancel_infinite_loop() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("while True: pass");

    // Cancel after a short delay
    let cancel_handle = handle.cancellation_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_handle.cancel();
    });

    let result = handle.wait().await;
    assert!(result.is_err(), "Should be cancelled");
    match result {
        Err(Error::Cancelled) => {} // Expected
        Err(e) => panic!("Expected Cancelled error, got: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}

#[tokio::test]
async fn test_execute_cancellable_cancel_immediately() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable(
        r#"
import time
for i in range(1000):
    time.sleep(0.01)
    print(f"Iteration {i}")
"#,
    );

    // Cancel immediately
    handle.cancel();

    let result = handle.wait().await;
    assert!(result.is_err(), "Should be cancelled");
    match result {
        Err(Error::Cancelled) => {} // Expected
        Err(e) => panic!("Expected Cancelled error, got: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}

#[tokio::test]
async fn test_is_running_before_and_after_completion() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("x = 1 + 1");

    // Should be running initially (or might complete very quickly)
    // This is a bit racy, so we just check that is_running doesn't panic
    let _ = handle.is_running();

    // Wait for completion
    let result = handle.wait().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_is_running_after_cancel() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("while True: pass");

    // Cancel
    handle.cancel();

    // After cancellation, is_running should return false
    // (because the token is cancelled, even if execution hasn't stopped yet)
    assert!(!handle.is_running(), "Should not be running after cancel");

    // Wait for the result
    let _ = handle.wait().await;
}

#[tokio::test]
async fn test_cancel_multiple_times_is_idempotent() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("while True: pass");

    // Cancel multiple times
    handle.cancel();
    handle.cancel();
    handle.cancel();

    let result = handle.wait().await;
    assert!(result.is_err(), "Should be cancelled");
    match result {
        Err(Error::Cancelled) => {} // Expected
        Err(e) => panic!("Expected Cancelled error, got: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}

#[tokio::test]
async fn test_cancellation_token_can_be_shared() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("while True: pass");

    // Get the cancellation token and use it from another task
    let token = handle.cancellation_token();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    let result = handle.wait().await;
    assert!(result.is_err(), "Should be cancelled via shared token");
    match result {
        Err(Error::Cancelled) => {} // Expected
        Err(e) => panic!("Expected Cancelled error, got: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}

// =============================================================================
// Cancellation with Python Execution States
// =============================================================================

#[tokio::test]
async fn test_cancel_during_python_computation() {
    let sandbox = sandbox_builder_with_short_timeout()
        .build()
        .expect("Failed to build sandbox");

    // Heavy computation that should be interruptible
    let handle = sandbox.execute_cancellable(
        r#"
result = 0
for i in range(10_000_000):
    result += i
print(result)
"#,
    );

    // Cancel after a short delay
    let token = handle.cancellation_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    let result = handle.wait().await;
    // Should either complete or be cancelled
    match result {
        Ok(_) => {}                 // Completed before cancellation
        Err(Error::Cancelled) => {} // Cancelled as expected
        Err(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_fast_execution_completes_before_cancel() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("print('fast')");

    // Schedule cancellation far in the future - execution should complete first
    // Use a long delay (10s) because CI can be slow with sandbox creation
    let token = handle.cancellation_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        token.cancel();
    });

    let result = handle.wait().await;
    assert!(
        result.is_ok(),
        "Fast execution should complete: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(output.stdout.contains("fast"));
}

// =============================================================================
// Error Cases
// =============================================================================

#[tokio::test]
async fn test_python_error_not_confused_with_cancellation() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("raise ValueError('test error')");

    let result = handle.wait().await;
    assert!(result.is_err(), "Should fail with Python error");
    match result {
        Err(Error::Cancelled) => panic!("Should not be Cancelled error"),
        Err(Error::Execution(msg)) => {
            assert!(
                msg.contains("ValueError") || msg.contains("test error"),
                "Error should mention ValueError: {}",
                msg
            );
        }
        Err(e) => panic!("Unexpected error type: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}

#[tokio::test]
async fn test_syntax_error_not_confused_with_cancellation() {
    let sandbox = sandbox_builder().build().expect("Failed to build sandbox");

    let handle = sandbox.execute_cancellable("def broken(");

    let result = handle.wait().await;
    assert!(result.is_err(), "Should fail with syntax error");
    match result {
        Err(Error::Cancelled) => panic!("Should not be Cancelled error"),
        Err(Error::Execution(_)) => {} // Expected
        Err(e) => panic!("Unexpected error type: {:?}", e),
        Ok(_) => panic!("Expected error, got success"),
    }
}
