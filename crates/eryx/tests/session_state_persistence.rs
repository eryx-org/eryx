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
        .execute("x = 42")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to execute x = 42: {}", e));
    assert_eq!(output.stdout, "", "Assignment should produce no output");

    // Access the variable in a subsequent call
    let output = session
        .execute("print(x)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to execute print(x): {}", e));
    assert_eq!(
        output.stdout, "42",
        "Variable x should persist and equal 42"
    );

    // Modify the variable
    let output = session
        .execute("x = x + 1")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to execute x = x + 1: {}", e));
    assert_eq!(output.stdout, "", "Assignment should produce no output");

    // Verify the modification persisted
    let output = session
        .execute("print(x)")
        .run()
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
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to define function: {}", e));
    assert_eq!(
        output.stdout, "",
        "Function definition should produce no output"
    );

    // Call the function in a subsequent execution
    let output = session
        .execute("print(greet('World'))")
        .run()
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
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to define class: {}", e));
    assert_eq!(
        output.stdout, "",
        "Class definition should produce no output"
    );

    // Create an instance
    let output = session
        .execute("counter = MyCounter(10)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to create instance: {}", e));
    assert_eq!(
        output.stdout, "",
        "Instance creation should produce no output"
    );

    // Call methods on the instance
    let output = session
        .execute("print(counter.increment())")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to call increment: {}", e));
    assert_eq!(output.stdout, "11", "First increment should return 11");

    let output = session
        .execute("print(counter.increment())")
        .run()
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
        .execute("x = 100")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));

    // Verify it exists
    let output = session
        .execute("print(x)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to print x: {}", e));
    assert_eq!(output.stdout, "100");

    // Clear the state
    session
        .clear_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to clear state: {}", e));

    // Variable should no longer exist
    let result = session.execute("print(x)").run().await;
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
        .execute("x = 100")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));

    // Verify it exists
    let output = session
        .execute("print(x)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to print x: {}", e));
    assert_eq!(output.stdout, "100");

    // Reset the session
    session
        .reset(&[])
        .await
        .unwrap_or_else(|e| panic!("Failed to reset session: {}", e));

    // Variable should no longer exist
    let result = session.execute("print(x)").run().await;
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
        .execute("data = []")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to create list: {}", e));

    session
        .execute("data.append(1)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to append 1: {}", e));

    session
        .execute("data.append(2)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to append 2: {}", e));

    session
        .execute("data.append(3)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to append 3: {}", e));

    // Verify the accumulated state
    let output = session
        .execute("print(sum(data))")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to sum data: {}", e));
    assert_eq!(output.stdout, "6", "Sum of [1, 2, 3] should be 6");

    let output = session
        .execute("print(len(data))")
        .run()
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
        .execute("x = 1")
        .run()
        .await
        .unwrap_or_else(|e| panic!("exec 1 failed: {}", e));
    assert_eq!(session.execution_count(), 1);

    session
        .execute("x = 2")
        .run()
        .await
        .unwrap_or_else(|e| panic!("exec 2 failed: {}", e));
    assert_eq!(session.execution_count(), 2);

    session
        .execute("x = 3")
        .run()
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
        .execute("x = 10")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to set x: {}", e));
    session
        .execute("y = 20")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to set y: {}", e));
    session
        .execute("data = [1, 2, 3]")
        .run()
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
        .execute("x = 999")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to modify x: {}", e));

    // Verify modification
    let output = session
        .execute("print(x)")
        .run()
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
        .execute("print(x)")
        .run()
        .await
        .expect("Failed to read x");
    assert_eq!(output.stdout, "10", "x should be restored to 10");

    let output = session
        .execute("print(y)")
        .run()
        .await
        .expect("Failed to read y");
    assert_eq!(output.stdout, "20", "y should be restored to 20");

    let output = session
        .execute("print(data)")
        .run()
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
        .execute("value = 42")
        .run()
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
        .execute("print(value)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to print value: {}", e));
    assert_eq!(output.stdout, "42");
}

/// Test that unserializable objects (e.g. modules) are skipped gracefully.
#[tokio::test]
async fn test_snapshot_with_unserializable() {
    let mut session = create_session().await;

    // Import a module (not serializable)
    session
        .execute("import sys")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to import sys: {}", e));

    // Also create a serializable variable
    session
        .execute("num = 100")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to set num: {}", e));

    // Snapshot should succeed, skipping the module
    let snapshot = session
        .snapshot_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to snapshot: {}", e));

    session.clear_state().await.unwrap();
    session.restore_state(&snapshot).await.unwrap();

    let output = session
        .execute("print(num)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to print num: {}", e));
    assert_eq!(output.stdout, "100", "num should be restored");
}

/// Test that user-defined functions survive snapshot/restore.
#[tokio::test]
async fn test_snapshot_with_functions() {
    let mut session = create_session().await;

    session
        .execute("def greet(name): return f'Hello, {name}!'")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to define function: {}", e));

    session
        .execute("fn = lambda x: x * 2")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to define lambda: {}", e));

    // --- CI diagnostic: inspect globals and dill serialization ---
    let diag = session
        .execute(
            r#"
import dill
import sys

lines = []
lines.append(f"Python: {sys.version}")
lines.append(f"dill version: {dill.__version__}")

g = globals()
user_keys = sorted([k for k in g.keys() if not k.startswith('__') and not k.startswith('_eryx')])
all_keys = sorted(g.keys())
lines.append(f"all globals keys: {all_keys}")
lines.append(f"user keys: {user_keys}")

# Check if callback infrastructure exists
for name in ['invoke', 'list_callbacks', '_EryxNamespace', '_EryxCallbackLeaf', '_eryx_make_callback', '_eryx_reserved']:
    lines.append(f"  {name} in globals: {name in g}")

# Try serializing each user item individually
for k in user_keys:
    v = g[k]
    try:
        data = dill.dumps(v)
        lines.append(f"OK: {k} ({type(v).__name__}, {len(data)} bytes)")
    except Exception as e:
        lines.append(f"FAIL: {k} ({type(v).__name__}): {type(e).__name__}: {e}")

# For functions, check __globals__ contents
for k in user_keys:
    v = g[k]
    if callable(v) and hasattr(v, '__globals__'):
        fg_keys = sorted(v.__globals__.keys())
        lines.append(f"  {k}.__globals__ keys: {fg_keys}")
        lines.append(f"  {k}.__globals__ is globals(): {v.__globals__ is g}")
        # Check if callback items are in __globals__
        for cb in ['invoke', 'list_callbacks', '_EryxNamespace']:
            lines.append(f"    '{cb}' in {k}.__globals__: {cb in v.__globals__}")

# Try the full stash-and-serialize approach manually
lines.append("--- manual stash test ---")
exclude = {'__builtins__', '__name__', '__doc__', '__package__', '__loader__', '__spec__', '__cached__', '__file__', 'invoke', 'list_callbacks', '_EryxNamespace', '_EryxCallbackLeaf', '_eryx_make_callback', '_eryx_reserved'}
stashed = {}
for sk in list(g.keys()):
    if sk in exclude:
        stashed[sk] = g.pop(sk)
try:
    remaining = sorted([k for k in g.keys() if not k.startswith('_eryx')])
    lines.append(f"after stash, remaining user keys: {remaining}")
    state_dict = {}
    for sk in remaining:
        sv = g.get(sk)
        if sv is not None:
            try:
                dill.dumps(sv)
                state_dict[sk] = sv
                lines.append(f"  stashed OK: {sk}")
            except Exception as e:
                lines.append(f"  stashed FAIL: {sk}: {type(e).__name__}: {e}")
    result = dill.dumps(state_dict)
    lines.append(f"final dict size: {len(result)} bytes, keys: {sorted(state_dict.keys())}")
finally:
    g.update(stashed)
lines.append(f"after restore, invoke in globals: {'invoke' in g}")

print('\n'.join(lines))
"#,
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Diagnostic failed: {}", e));
    eprintln!(
        "=== SNAPSHOT DIAGNOSTIC ===\n{}\n=== END DIAGNOSTIC ===",
        diag.stdout
    );
    // --- end diagnostic ---

    let snapshot = session
        .snapshot_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to snapshot: {}", e));
    assert!(
        snapshot.size() > 10,
        "Snapshot should contain serialized data, got {} bytes",
        snapshot.size()
    );

    session.clear_state().await.unwrap();

    session
        .restore_state(&snapshot)
        .await
        .unwrap_or_else(|e| panic!("Failed to restore: {}", e));

    let output = session
        .execute("print(greet('World'))")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to call greet: {}", e));
    assert_eq!(output.stdout, "Hello, World!");

    let output = session
        .execute("print(fn(21))")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to call lambda: {}", e));
    assert_eq!(output.stdout, "42");
}

/// Test that user-defined classes and instances survive snapshot/restore.
#[tokio::test]
async fn test_snapshot_with_classes() {
    let mut session = create_session().await;

    session
        .execute(
            r#"
class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y
    def distance(self):
        return (self.x**2 + self.y**2) ** 0.5
"#,
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to define class: {}", e));

    session
        .execute("p = Point(3, 4)")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to create instance: {}", e));

    // --- CI diagnostic for class snapshot ---
    let diag = session
        .execute(
            r#"
import dill
lines = []
g = globals()
user_keys = sorted([k for k in g.keys() if not k.startswith('__') and not k.startswith('_eryx')])
lines.append(f"user keys: {user_keys}")
for k in user_keys:
    v = g[k]
    try:
        data = dill.dumps(v)
        lines.append(f"OK: {k} ({type(v).__name__}, {len(data)} bytes)")
    except Exception as e:
        lines.append(f"FAIL: {k} ({type(v).__name__}): {type(e).__name__}: {e}")
        # For classes/instances, try to understand WHY serialization fails
        if hasattr(v, '__dict__'):
            for attr_name, attr_val in vars(v).items():
                try:
                    dill.dumps(attr_val)
                    lines.append(f"  attr OK: {attr_name}")
                except Exception as e2:
                    lines.append(f"  attr FAIL: {attr_name}: {type(e2).__name__}: {e2}")
print('\n'.join(lines))
"#,
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Class diagnostic failed: {}", e));
    eprintln!(
        "=== CLASS SNAPSHOT DIAGNOSTIC ===\n{}\n=== END DIAGNOSTIC ===",
        diag.stdout
    );
    // --- end diagnostic ---

    let snapshot = session
        .snapshot_state()
        .await
        .unwrap_or_else(|e| panic!("Failed to snapshot: {}", e));
    eprintln!("Class snapshot size: {} bytes", snapshot.size());

    session.clear_state().await.unwrap();

    session
        .restore_state(&snapshot)
        .await
        .unwrap_or_else(|e| panic!("Failed to restore: {}", e));

    // --- Diagnostic: check what's in globals after restore ---
    let diag2 = session
        .execute(
            r#"
g = globals()
user_keys = sorted([k for k in g.keys() if not k.startswith('__') and not k.startswith('_eryx')])
print(f"after restore user keys: {user_keys}")
for k in user_keys:
    print(f"  {k}: {type(g[k]).__name__}")
"#,
        )
        .run()
        .await
        .unwrap_or_else(|e| panic!("Post-restore diagnostic failed: {}", e));
    eprintln!(
        "=== POST-RESTORE DIAGNOSTIC ===\n{}\n=== END DIAGNOSTIC ===",
        diag2.stdout
    );

    // Instance should work
    let output = session
        .execute("print(p.distance())")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to call method: {}", e));
    assert_eq!(output.stdout, "5.0");

    // Can create new instances of the restored class
    let output = session
        .execute("print(Point(5, 12).distance())")
        .run()
        .await
        .unwrap_or_else(|e| panic!("Failed to create new instance: {}", e));
    assert_eq!(output.stdout, "13.0");
}
