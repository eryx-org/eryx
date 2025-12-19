//! Integration test for SessionExecutor state persistence.
//!
//! This test verifies that Python state (variables, functions, etc.) persists
//! between execute() calls when using SessionExecutor.
#![allow(clippy::unwrap_used, clippy::expect_used)]
//!
//! ## Running Tests
//!
//! Use `mise run test` which automatically handles precompilation:
//! ```sh
//! mise run setup  # One-time: build WASM + precompile
//! mise run test   # Run tests with precompiled WASM (~0.1s)
//! ```
//!
//! Or manually with cargo:
//! ```sh
//! cargo nextest run --workspace --features precompiled
//! ```

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eryx::{PythonExecutor, PythonStateSnapshot, SessionExecutor};

/// Shared executor to avoid repeated WASM loading across tests.
/// With precompiled WASM (the default), loading takes ~50ms.
static SHARED_EXECUTOR: OnceLock<Arc<PythonExecutor>> = OnceLock::new();

fn get_shared_executor() -> Arc<PythonExecutor> {
    SHARED_EXECUTOR
        .get_or_init(|| Arc::new(create_executor()))
        .clone()
}

/// Create a PythonExecutor, using embedded resources if available.
fn create_executor() -> PythonExecutor {
    // When embedded feature is available, use it (more reliable)
    #[cfg(feature = "embedded")]
    {
        use eryx::embedded::EmbeddedResources;
        let resources = EmbeddedResources::get().expect("Failed to extract embedded resources");
        // SAFETY: We trust the embedded precompiled runtime
        #[allow(unsafe_code)]
        return unsafe {
            PythonExecutor::from_precompiled_file(&resources.runtime_path)
                .expect("Failed to load embedded runtime")
                .with_python_stdlib(&resources.stdlib_path)
        };
    }

    // Fallback for when embedded feature is not available
    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        let path = runtime_wasm_path();
        PythonExecutor::from_file(&path)
            .unwrap_or_else(|e| panic!("Failed to load runtime.wasm from {:?}: {}", path, e))
            .with_python_stdlib(&stdlib_path)
    }
}

/// Get the path to runtime.wasm relative to the workspace root.
#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    // CARGO_MANIFEST_DIR points to crates/eryx
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

/// Helper to create a session executor for tests.
/// Uses a shared PythonExecutor to avoid repeated WASM compilation.
async fn create_session() -> SessionExecutor {
    let executor = get_shared_executor();

    SessionExecutor::new(executor, &[])
        .await
        .unwrap_or_else(|e| panic!("Failed to create session: {}", e))
}

/// Test that variables persist between execute() calls.
#[tokio::test]
async fn test_variable_persistence() {
    let mut session = create_session().await;

    // Define a variable
    let output = session
        .execute("x = 42", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to execute x = 42: {}", e));
    assert_eq!(output.stdout, "", "Assignment should produce no output");

    // Access the variable in a subsequent call
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to execute print(x): {}", e));
    assert_eq!(
        output.stdout, "42",
        "Variable x should persist and equal 42"
    );

    // Modify the variable
    let output = session
        .execute("x = x + 1", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to execute x = x + 1: {}", e));
    assert_eq!(output.stdout, "", "Assignment should produce no output");

    // Verify the modification persisted
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to execute print(x) after modification: {}", e));
    assert_eq!(
        output.stdout, "43",
        "Variable x should be 43 after increment"
    );
}

/// Test that functions persist between execute() calls.
#[tokio::test]
async fn test_function_persistence() {
    let mut session = create_session().await;

    // Define a function
    let output = session
        .execute(
            r#"
def greet(name):
    return f"Hello, {name}!"
"#,
            &[],
            None,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("Failed to define function: {}", e));
    assert_eq!(
        output.stdout, "",
        "Function definition should produce no output"
    );

    // Call the function in a subsequent execution
    let output = session
        .execute("print(greet('World'))", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to call greet function: {}", e));
    assert_eq!(
        output.stdout, "Hello, World!",
        "Function should be callable"
    );
}

/// Test that classes persist between execute() calls.
#[tokio::test]
async fn test_class_persistence() {
    let mut session = create_session().await;

    // Define a class (use MyCounter to avoid conflict with collections.Counter)
    let output = session
        .execute(
            r#"
class MyCounter:
    def __init__(self, start=0):
        self.value = start

    def increment(self):
        self.value += 1
        return self.value
"#,
            &[],
            None,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("Failed to define class: {}", e));
    assert_eq!(
        output.stdout, "",
        "Class definition should produce no output"
    );

    // Create an instance
    let output = session
        .execute("counter = MyCounter(10)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to create instance: {}", e));
    assert_eq!(
        output.stdout, "",
        "Instance creation should produce no output"
    );

    // Call methods on the instance
    let output = session
        .execute("print(counter.increment())", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to call increment: {}", e));
    assert_eq!(output.stdout, "11", "First increment should return 11");

    let output = session
        .execute("print(counter.increment())", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to call second increment: {}", e));
    assert_eq!(output.stdout, "12", "Second increment should return 12");
}

/// Test that clear_state() clears persistent variables.
#[tokio::test]
async fn test_clear_state() {
    let mut session = create_session().await;

    // Define a variable
    session
        .execute("x = 100", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));

    // Verify it exists
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to print x: {}", e));
    assert_eq!(output.stdout, "100");

    // Clear the state
    session
        .clear_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to clear state: {}", e));

    // Variable should no longer exist
    let result = session.execute("print(x)", &[], None, None).await;
    assert!(
        result.is_err(),
        "After clear_state, x should not be defined: {:?}",
        result
    );
}

/// Test that reset() clears state by creating a new WASM instance.
#[tokio::test]
async fn test_reset_clears_state() {
    let mut session = create_session().await;

    // Define a variable
    session
        .execute("x = 100", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));

    // Verify it exists
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to print x: {}", e));
    assert_eq!(output.stdout, "100");

    // Reset the session
    session
        .reset(&[])
        .await
        .unwrap_or_else(|e| panic!("Failed to reset session: {}", e));

    // Variable should no longer exist
    let result = session.execute("print(x)", &[], None, None).await;
    assert!(
        result.is_err(),
        "After reset, x should not be defined: {:?}",
        result
    );
}

/// Test multiple variables and complex state.
#[tokio::test]
async fn test_complex_state_persistence() {
    let mut session = create_session().await;

    // Build up complex state across multiple calls
    session
        .execute("data = []", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to create list: {}", e));

    session
        .execute("data.append(1)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to append 1: {}", e));

    session
        .execute("data.append(2)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to append 2: {}", e));

    session
        .execute("data.append(3)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to append 3: {}", e));

    // Verify the accumulated state
    let output = session
        .execute("print(sum(data))", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to sum data: {}", e));
    assert_eq!(output.stdout, "6", "Sum of [1, 2, 3] should be 6");

    let output = session
        .execute("print(len(data))", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to get len: {}", e));
    assert_eq!(output.stdout, "3", "Length should be 3");
}

/// Test execution count tracking.
#[tokio::test]
async fn test_execution_count() {
    let mut session = create_session().await;

    assert_eq!(session.execution_count(), 0, "Initial count should be 0");

    session
        .execute("x = 1", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("exec 1 failed: {}", e));
    assert_eq!(session.execution_count(), 1);

    session
        .execute("x = 2", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("exec 2 failed: {}", e));
    assert_eq!(session.execution_count(), 2);

    session
        .execute("x = 3", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("exec 3 failed: {}", e));
    assert_eq!(session.execution_count(), 3);

    // Reset should clear count
    session
        .reset(&[])
        .await
        .unwrap_or_else(|e| panic!("reset failed: {}", e));
    assert_eq!(
        session.execution_count(),
        0,
        "Count should be 0 after reset"
    );
}

/// Test state snapshot and restore.
#[tokio::test]
async fn test_snapshot_and_restore() {
    let mut session = create_session().await;

    // Build up some state
    session
        .execute("x = 10", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));
    session
        .execute("y = 20", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set y: {}", e));
    session
        .execute("data = [1, 2, 3]", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set data: {}", e));

    // Take a snapshot
    let snapshot = session
        .snapshot_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to snapshot: {}", e));

    assert!(snapshot.size() > 0, "Snapshot should have data");

    // Modify the state
    session
        .execute("x = 999", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to modify x: {}", e));

    // Verify modification
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to print x: {}", e));
    assert_eq!(output.stdout, "999");

    // Restore the snapshot
    session
        .restore_state(&snapshot)
        .await
        .unwrap_or_else(|e| panic!("Failed to restore: {}", e));

    // Verify original values are back
    let output = session
        .execute("print(x)", &[], None, None)
        .await
        .expect("Failed to read x");
    assert_eq!(output.stdout, "10", "x should be restored to 10");

    let output = session
        .execute("print(y)", &[], None, None)
        .await
        .expect("Failed to read y");
    assert_eq!(output.stdout, "20", "y should be restored to 20");

    let output = session
        .execute("print(data)", &[], None, None)
        .await
        .expect("Failed to read data");
    assert_eq!(output.stdout, "[1, 2, 3]", "data should be restored");
}

/// Test snapshot serialization roundtrip.
#[tokio::test]
async fn test_snapshot_serialization() {
    let mut session = create_session().await;

    // Set up state
    session
        .execute("value = 42", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set value: {}", e));

    // Take a snapshot
    let snapshot = session
        .snapshot_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to snapshot: {}", e));

    // Serialize to bytes
    let bytes = snapshot.to_bytes();
    assert!(
        bytes.len() > 8,
        "Serialized bytes should include header + data"
    );

    // Deserialize
    let restored_snapshot = PythonStateSnapshot::from_bytes(&bytes)
        .unwrap_or_else(|e| panic!("Failed to deserialize: {}", e));

    assert_eq!(restored_snapshot.size(), snapshot.size());
    assert_eq!(
        restored_snapshot.metadata().timestamp_ms,
        snapshot.metadata().timestamp_ms
    );

    // Clear state and restore from deserialized snapshot
    session
        .clear_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to clear: {}", e));

    session
        .restore_state(&restored_snapshot)
        .await
        .unwrap_or_else(|e| panic!("Failed to restore from deserialized: {}", e));

    // Verify the value is back
    let output = session
        .execute("print(value)", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to print value: {}", e));
    assert_eq!(output.stdout, "42");
}

/// Test that unpicklable objects are handled gracefully.
#[tokio::test]
async fn test_snapshot_with_unpicklable() {
    let mut session = create_session().await;

    // Create a lambda (unpicklable)
    session
        .execute("fn = lambda x: x * 2", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to create lambda: {}", e));

    // Also create a picklable variable
    session
        .execute("num = 100", &[], None, None)
        .await
        .unwrap_or_else(|e| panic!("Failed to set num: {}", e));

    // Snapshot should still work, just skip the unpicklable lambda
    let snapshot = session.snapshot_state().await;

    // The snapshot might succeed (skipping unpicklable) or fail
    // Either behavior is acceptable
    if let Ok(snap) = snapshot {
        // If it succeeded, verify we can restore
        session.clear_state().await.unwrap();
        session.restore_state(&snap).await.unwrap();

        // num should be restored
        let result = session.execute("print(num)", &[], None, None).await;
        assert!(result.is_ok(), "num should be restored");
        let output = result.unwrap();
        assert_eq!(output.stdout, "100", "num should equal 100");
    }
    // If snapshot failed, that's also acceptable behavior
}
