//! Integration tests for Python error handling scenarios.
//!
//! These tests verify that Python errors (syntax errors, runtime errors, etc.)
//! are properly propagated and reported through the SessionExecutor.
//!
//! Note: Tests for callback error handling require setting up callback channels
//! and handlers, which is handled by the higher-level Sandbox API. Those tests
//! would be better placed in example code or Sandbox-level integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! ## Running Tests
//!
//! Use `mise run test` which automatically handles precompilation:
//! ```sh
//! mise run setup  # One-time: build WASM + precompile
//! mise run test   # Run tests with precompiled WASM (~0.1s)
//! ```

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
    // When embedded feature is enabled, use it for zero-config setup
    #[cfg(feature = "embedded")]
    {
        let resources =
            eryx::embedded::EmbeddedResources::get().expect("Failed to extract embedded resources");

        #[allow(unsafe_code)]
        unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
            .expect("Failed to load embedded runtime")
            .with_python_stdlib(resources.stdlib())
    }

    // Fall back to file-based loading
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

/// Helper to create a session for testing.
async fn create_session() -> SessionExecutor {
    let executor = get_shared_executor();

    SessionExecutor::new(executor, &[])
        .await
        .expect("Failed to create session")
}

// =============================================================================
// Python Syntax Error Tests
// =============================================================================

#[tokio::test]
async fn test_python_syntax_error_missing_colon() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
def broken()  # Missing colon
    pass
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with syntax error");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("SyntaxError") || error.contains("syntax"),
        "Error should mention syntax: {}",
        error
    );
}

#[tokio::test]
async fn test_python_syntax_error_unclosed_parenthesis() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
print("hello"
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with syntax error");
}

#[tokio::test]
async fn test_python_syntax_error_invalid_indentation() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
def foo():
pass  # Should be indented
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with indentation error");
}

// =============================================================================
// Python Runtime Error Tests
// =============================================================================

#[tokio::test]
async fn test_python_name_error_undefined_variable() {
    let mut session = create_session().await;

    let result = session
        .execute("x = undefined_variable + 1", &[], None, None)
        .await;

    assert!(result.is_err(), "Should fail with NameError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("NameError") || error.contains("undefined"),
        "Error should mention undefined variable: {}",
        error
    );
}

#[tokio::test]
async fn test_python_type_error_string_plus_int() {
    let mut session = create_session().await;

    let result = session
        .execute(r#"result = "hello" + 42"#, &[], None, None)
        .await;

    assert!(result.is_err(), "Should fail with TypeError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("TypeError") || error.contains("type"),
        "Error should mention type error: {}",
        error
    );
}

#[tokio::test]
async fn test_python_zero_division_error() {
    let mut session = create_session().await;

    let result = session.execute("x = 42 / 0", &[], None, None).await;

    assert!(result.is_err(), "Should fail with ZeroDivisionError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("ZeroDivisionError") || error.contains("division"),
        "Error should mention division: {}",
        error
    );
}

#[tokio::test]
async fn test_python_index_error() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
items = [1, 2, 3]
x = items[10]
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with IndexError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("IndexError") || error.contains("index"),
        "Error should mention index: {}",
        error
    );
}

#[tokio::test]
async fn test_python_key_error() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
data = {"a": 1}
x = data["nonexistent"]
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with KeyError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("KeyError") || error.contains("key"),
        "Error should mention key: {}",
        error
    );
}

#[tokio::test]
async fn test_python_attribute_error() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
x = 42
x.nonexistent_method()
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "Should fail with AttributeError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("AttributeError") || error.contains("attribute"),
        "Error should mention attribute: {}",
        error
    );
}

#[tokio::test]
async fn test_python_value_error() {
    let mut session = create_session().await;

    let result = session
        .execute(r#"x = int("not_a_number")"#, &[], None, None)
        .await;

    assert!(result.is_err(), "Should fail with ValueError");
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("ValueError") || error.contains("invalid"),
        "Error should mention value error: {}",
        error
    );
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[tokio::test]
async fn test_empty_code_executes_successfully() {
    let mut session = create_session().await;

    let result = session.execute("", &[], None, None).await;

    assert!(result.is_ok(), "Empty code should execute successfully");
    let output = result.unwrap();
    assert!(output.stdout.is_empty(), "Empty code produces no output");
}

#[tokio::test]
async fn test_whitespace_only_code() {
    let mut session = create_session().await;

    let result = session.execute("   \n\n   \t\t\n", &[], None, None).await;

    assert!(
        result.is_ok(),
        "Whitespace-only code should execute successfully"
    );
}

#[tokio::test]
async fn test_comment_only_code() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
# This is a comment
# Another comment
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_ok(), "Comment-only code should succeed");
}

#[tokio::test]
async fn test_unicode_in_code() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
message = "Hello 世界 🌍 مرحبا"
print(message)
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_ok(), "Unicode should work");
    let output = result.unwrap();
    assert!(output.stdout.contains("世界"), "Should contain Chinese");
    assert!(output.stdout.contains("🌍"), "Should contain emoji");
}

#[tokio::test]
async fn test_exception_in_except_block() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
try:
    x = 1 / 0
except:
    # Another error in the except block
    y = undefined_in_except
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "Exception in except block should propagate"
    );
}

#[tokio::test]
async fn test_multiple_errors_in_sequence() {
    let mut session = create_session().await;

    // First error
    let result1 = session.execute("x = 1 / 0", &[], None, None).await;
    assert!(result1.is_err());

    // Session should still work after error
    let result2 = session.execute("print('recovered')", &[], None, None).await;
    assert!(result2.is_ok());
    assert!(result2.unwrap().stdout.contains("recovered"));

    // Another error
    let result3 = session.execute("y = undefined_var", &[], None, None).await;
    assert!(result3.is_err());

    // And recover again
    let result4 = session
        .execute("print('still working')", &[], None, None)
        .await;
    assert!(result4.is_ok());
}

#[tokio::test]
async fn test_large_output() {
    let mut session = create_session().await;

    let result = session
        .execute(
            r#"
for i in range(1000):
    print(f"Line {i}: " + "x" * 100)
"#,
            &[],
            None,
            None,
        )
        .await;

    assert!(result.is_ok(), "Large output should work");
    let output = result.unwrap();
    assert!(
        output.stdout.len() > 100_000,
        "Should have substantial output"
    );
    assert!(output.stdout.contains("Line 999"), "Should have last line");
}
