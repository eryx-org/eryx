//! Integration tests for pre-initialization.
//!
//! These tests verify that pre-initialization works correctly and that
//! arbitrary imports work after pre-init (i.e., the WASI reset doesn't
//! break normal import functionality at runtime).
//!
//! These tests require the `preinit` feature.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "preinit")]

use eryx::Sandbox;
use eryx::preinit::pre_initialize;
use std::path::PathBuf;
use std::sync::Arc;

/// Get the path to the Python stdlib for tests.
fn get_stdlib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("eryx-wasm-runtime/tests/python-stdlib")
}

// =============================================================================
// Pre-initialization Tests
// =============================================================================

/// Test that pre-initialization completes without errors.
#[tokio::test]
async fn preinit_basic() {
    let stdlib = get_stdlib_path();

    // Pre-initialize with no imports
    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    // Verify we got valid component bytes
    assert!(!preinit_bytes.is_empty());
    // WASM components start with \0asm
    assert_eq!(&preinit_bytes[0..4], b"\0asm");
}

/// Test that pre-initialized component can execute code.
#[tokio::test]
async fn preinit_can_execute() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    // Create sandbox from pre-initialized bytes
    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");

    let result = sandbox
        .execute("print('hello from preinit')")
        .await
        .expect("execution should succeed");

    assert!(result.stdout.contains("hello from preinit"));
}

/// Test that arbitrary stdlib imports work after pre-initialization.
///
/// This is critical: the WASI reset at the end of pre-init clears file handles,
/// but should NOT prevent new imports from working at runtime.
#[tokio::test]
async fn preinit_arbitrary_imports_work() {
    let stdlib = get_stdlib_path();

    // Pre-initialize with NO imports (empty list)
    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");

    // Test importing various stdlib modules that were NOT pre-imported
    let result = sandbox
        .execute(
            r#"
import json
import base64
import hashlib
import re
import collections

# Verify they all work
print(f"json: {json.dumps({'a': 1})}")
print(f"base64: {base64.b64encode(b'test').decode()}")
print(f"hashlib: {hashlib.md5(b'test').hexdigest()[:8]}")
print(f"re: {re.match(r'\d+', '123').group()}")
print(f"collections: {type(collections.OrderedDict()).__name__}")
"#,
        )
        .await
        .expect("imports should work");

    assert!(result.stdout.contains(r#"json: {"a": 1}"#));
    assert!(result.stdout.contains("base64: dGVzdA==")); // base64 of 'test'
    assert!(result.stdout.contains("hashlib: 098f6bcd")); // md5 prefix
    assert!(result.stdout.contains("re: 123"));
    assert!(result.stdout.contains("collections: OrderedDict"));
}

/// Test that multiple sandboxes can be created from the same pre-init bytes.
#[tokio::test]
async fn preinit_multiple_sandboxes() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = Arc::new(
        pre_initialize(&stdlib, None, &[], &[])
            .await
            .expect("pre-initialization should succeed"),
    );

    // Create multiple sandboxes from the same pre-init bytes
    for i in 0..3 {
        let sandbox = Sandbox::builder()
            .with_wasm_bytes((*preinit_bytes).clone())
            .with_python_stdlib(&stdlib)
            .build()
            .expect("sandbox creation should succeed");

        let result = sandbox
            .execute(&format!("print('sandbox {i}')"))
            .await
            .expect("execution should succeed");

        assert!(result.stdout.contains(&format!("sandbox {i}")));
    }
}

/// Test that sandboxes from pre-init are isolated from each other.
#[tokio::test]
async fn preinit_sandboxes_isolated() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    // Create first sandbox and set a variable
    let sandbox1 = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes.clone())
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    sandbox1
        .execute("secret_value = 'sandbox1_secret'")
        .await
        .unwrap();

    // Create second sandbox - should NOT see the variable
    let sandbox2 = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    let result = sandbox2
        .execute(
            r#"
try:
    print(f"found: {secret_value}")
except NameError:
    print("variable not found - correctly isolated")
"#,
        )
        .await
        .unwrap();

    assert!(result.stdout.contains("correctly isolated"));
}

/// Test pre-initialization with imports specified.
#[tokio::test]
async fn preinit_with_imports() {
    let stdlib = get_stdlib_path();

    // Pre-initialize with json module imported
    let preinit_bytes = pre_initialize(&stdlib, None, &["json"], &[])
        .await
        .expect("pre-initialization should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");

    // json should already be in sys.modules
    let result = sandbox
        .execute(
            r#"
import sys
if 'json' in sys.modules:
    print("json was pre-imported")
else:
    print("json not in sys.modules")

# Should still work
import json
print(json.dumps([1, 2, 3]))
"#,
        )
        .await
        .expect("execution should succeed");

    assert!(result.stdout.contains("json was pre-imported"));
    assert!(result.stdout.contains("[1, 2, 3]"));
}

/// Test that imports work within a single execute call (multi-statement).
#[tokio::test]
async fn preinit_imports_work_within_execution() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    // Import and use within same execution
    let result = sandbox
        .execute(
            r#"
import json
import hashlib
print(json.dumps({'works': True}))
print(hashlib.md5(b'test').hexdigest()[:8])
"#,
        )
        .await
        .unwrap();

    assert!(result.stdout.contains(r#"{"works": true}"#));
    assert!(result.stdout.contains("098f6bcd"));
}

/// Test that file operations work after pre-init (WASI is functional).
#[tokio::test]
async fn preinit_file_operations_work() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[])
        .await
        .expect("pre-initialization should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    // Test that we can read files (the stdlib directory is mounted)
    let result = sandbox
        .execute(
            r#"
import os
# List contents of stdlib (should have some .py files)
files = os.listdir('/python-stdlib')
py_files = [f for f in files if f.endswith('.py') or not '.' in f]
print(f"found {len(py_files)} items")
print(f"has_encodings: {'encodings' in files}")
"#,
        )
        .await
        .unwrap();

    assert!(result.stdout.contains("has_encodings: True"));
}
