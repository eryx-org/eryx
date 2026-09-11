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
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

/// Get the path to the Python stdlib for tests.
fn get_stdlib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("eryx-wasm-runtime/tests/python-stdlib")
}

// =============================================================================
// Shared pre-initialization
// =============================================================================
//
// `pre_initialize` costs ~20s of CPU per call and saturates several cores on
// its own (wizer instrumentation plus a full wasmtime compile of a 34MB
// component). Every test below needs pre-initialized bytes, so without sharing
// the group redoes that work once per test and thrashes CPU against itself and
// the rest of the suite - enough to hit nextest's kill threshold in CI.
//
// Sharing has to work *across processes*: nextest runs each test in its own
// process, so an in-process cache alone would share nothing under it. The bytes
// are therefore cached on disk, keyed by nextest's per-run id so an entry can
// never outlive the build that produced it. `.config/nextest.toml` puts these
// tests in a single-threaded test group, so the first test computes the bytes
// and the rest just read the file. The trade-off is attribution: a failing
// pre-init is reported against whichever test happened to run first, not
// necessarily `preinit_basic`.

/// In-process cache, keyed by the pre-init arguments.
///
/// This is what shares the bytes under plain `cargo test`, where the whole
/// binary is one process running tests on threads.
static PREINIT_CACHE: OnceCell<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceCell::const_new();

/// Cache key for a set of pre-init imports, also used as the file name.
fn cache_key(imports: &[&str]) -> String {
    if imports.is_empty() {
        "default".to_string()
    } else {
        format!("imports-{}", imports.join("-"))
    }
}

/// Directory holding the on-disk pre-init cache for the current test run.
///
/// Keyed by `NEXTEST_RUN_ID` under nextest (one id shared by every test process
/// in a run) and by pid otherwise, so entries are always fresh with respect to
/// the runtime that produced them - a stale entry would silently test the wrong
/// component.
fn cache_dir() -> PathBuf {
    let run_id =
        std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("preinit-cache")
        .join(run_id)
}

/// Delete cache directories left behind by earlier runs.
///
/// Each entry is tens of megabytes and nothing else cleans them up. Only
/// directories older than an hour are touched, so a concurrent run's cache is
/// never pulled out from under it. Best-effort: failures are irrelevant to the
/// tests.
fn prune_stale_caches(current: &Path) {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

    let Some(root) = current.parent() else { return };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|m| m.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > MAX_AGE);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Pre-initialize once per test run and share the resulting bytes.
///
/// Callers passing different `imports` get their own entry, since those produce
/// genuinely different components.
async fn shared_preinit(stdlib: &Path, imports: &[&str]) -> Arc<Vec<u8>> {
    let key = cache_key(imports);

    let cache = PREINIT_CACHE
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await;
    let mut cache = cache.lock().await;
    if let Some(bytes) = cache.get(&key) {
        return Arc::clone(bytes);
    }

    let dir = cache_dir();
    let path = dir.join(format!("{key}.wasm"));

    let bytes = if let Ok(bytes) = std::fs::read(&path) {
        Arc::new(bytes)
    } else {
        let bytes = Arc::new(
            pre_initialize(stdlib, None, imports, &[], None)
                .await
                .expect("pre-initialization should succeed"),
        );

        // Publish via rename so a concurrent reader never sees a partial file.
        // Two processes racing here just both do the work, which is no worse
        // than not caching at all.
        std::fs::create_dir_all(&dir).expect("cache directory should be creatable");
        prune_stale_caches(&dir);
        let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes.as_slice()).expect("cache entry should be writable");
        std::fs::rename(&tmp, &path).expect("cache entry should be publishable");

        bytes
    };

    cache.insert(key, Arc::clone(&bytes));
    bytes
}

// =============================================================================
// Pre-initialization Tests
// =============================================================================

/// Test that pre-initialization completes without errors.
#[tokio::test]
async fn preinit_basic() {
    let stdlib = get_stdlib_path();

    // Pre-initialize with no imports
    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    // Verify we got valid component bytes
    assert!(!preinit_bytes.is_empty());
    // WASM components start with \0asm
    assert_eq!(&preinit_bytes[0..4], b"\0asm");
}

/// Test that pre-initialized component can execute code.
#[tokio::test]
async fn preinit_can_execute() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    // Create sandbox from pre-initialized bytes
    let sandbox = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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
    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    let sandbox = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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

    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

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

    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    // Create first sandbox and set a variable
    let sandbox1 = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    sandbox1
        .execute("secret_value = 'sandbox1_secret'")
        .await
        .unwrap();

    // Create second sandbox - should NOT see the variable
    let sandbox2 = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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

    // Pre-initialize with json module imported.
    // This needs its own pre-init: different imports, different component.
    let preinit_bytes = shared_preinit(&stdlib, &["json"]).await;

    let sandbox = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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

    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    let sandbox = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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

    let preinit_bytes = shared_preinit(&stdlib, &[]).await;

    let sandbox = Sandbox::builder()
        .with_wasm_bytes((*preinit_bytes).clone())
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

// =============================================================================
// Setup Code Tests
// =============================================================================

/// Test that setup_code runs during pre-init and state is available in sandboxes.
#[tokio::test]
async fn preinit_setup_code_defines_variable() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(
        &stdlib,
        None,
        &[],
        &[],
        Some("setup_value = 42\nsetup_list = [1, 2, 3]"),
    )
    .await
    .expect("pre-initialization with setup code should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");

    let result = sandbox
        .execute("print(f'{setup_value} {setup_list}')")
        .await
        .expect("execution should succeed");

    assert!(result.stdout.contains("42 [1, 2, 3]"));
}

/// Test that setup_code runs after imports, so imported modules are available.
#[tokio::test]
async fn preinit_setup_code_uses_imports() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(
        &stdlib,
        None,
        &["json"],
        &[],
        Some("precomputed = json.dumps({'ready': True})"),
    )
    .await
    .expect("pre-initialization with imports + setup code should succeed");

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");

    let result = sandbox
        .execute("print(precomputed)")
        .await
        .expect("execution should succeed");

    assert!(result.stdout.contains(r#"{"ready": true}"#));
}

/// Test that setup_code state is isolated between sandboxes (COW).
#[tokio::test]
async fn preinit_setup_code_isolated_between_sandboxes() {
    let stdlib = get_stdlib_path();

    let preinit_bytes = pre_initialize(&stdlib, None, &[], &[], Some("counter = 0"))
        .await
        .expect("pre-initialization should succeed");

    // First sandbox: mutate the variable
    let sandbox1 = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes.clone())
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    sandbox1
        .execute("counter += 100\nprint(f'sb1: {counter}')")
        .await
        .unwrap();

    // Second sandbox: should see the original value, not the mutated one
    let sandbox2 = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .unwrap();

    let result = sandbox2.execute("print(f'sb2: {counter}')").await.unwrap();

    assert!(result.stdout.contains("sb2: 0"));
}

/// Test that setup_code errors are reported clearly.
#[tokio::test]
async fn preinit_setup_code_error_is_reported() {
    let stdlib = get_stdlib_path();

    let result = pre_initialize(
        &stdlib,
        None,
        &[],
        &[],
        Some("raise ValueError('setup failed')"),
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("setup code"),
        "error should mention 'setup code': {err}"
    );
}

// =============================================================================
// Baked Callback Tests
// =============================================================================

/// A callback that returns its arguments.
struct Echo;

impl eryx::Callback for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Returns its arguments"
    }
    fn parameters_schema(&self) -> eryx::Schema {
        eryx::empty_schema()
    }
    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<serde_json::Value, eryx::CallbackError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(args) })
    }
}

/// A callback that answers "pong".
struct Ping;

impl eryx::Callback for Ping {
    fn name(&self) -> &str {
        "ping"
    }
    fn description(&self) -> &str {
        "Answers pong"
    }
    fn parameters_schema(&self) -> eryx::Schema {
        eryx::empty_schema()
    }
    fn invoke(
        &self,
        _args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<serde_json::Value, eryx::CallbackError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(serde_json::json!("pong")) })
    }
}

/// Pre-initialize with the given callbacks' declarations baked in.
async fn preinit_with_callbacks(stdlib: &Path, callbacks: &[&dyn eryx::Callback]) -> Vec<u8> {
    use eryx::preinit::{PreInitOptions, callback_declaration, pre_initialize_with_options};

    let declarations = callbacks
        .iter()
        .map(|cb| callback_declaration(*cb))
        .collect();
    pre_initialize_with_options(PreInitOptions::new(stdlib).callbacks(declarations))
        .await
        .expect("pre-initialization with callbacks should succeed")
}

/// Callbacks baked into the snapshot are callable from a sandbox registering
/// the same set, and a sandbox registering a different set gets that set.
#[tokio::test]
async fn preinit_baked_callbacks() {
    let stdlib = get_stdlib_path();
    let preinit_bytes = preinit_with_callbacks(&stdlib, &[&Echo]).await;

    // Same set as baked: the wrapper is already installed and works.
    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes.clone())
        .with_python_stdlib(&stdlib)
        .with_callback(Echo)
        .build()
        .expect("sandbox creation should succeed");
    let result = sandbox
        .execute(
            "print(sorted(c['name'] for c in list_callbacks()))\nprint((await echo(data=7))['data'])",
        )
        .await
        .expect("execution should succeed");
    assert_eq!(result.stdout.trim(), "['echo']\n7");

    // A different set is installed as before, replacing the baked one.
    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes.clone())
        .with_python_stdlib(&stdlib)
        .with_callback(Ping)
        .build()
        .expect("sandbox creation should succeed");
    let result = sandbox
        .execute("print(sorted(c['name'] for c in list_callbacks()), await ping())")
        .await
        .expect("execution should succeed");
    assert_eq!(result.stdout.trim(), "['ping'] pong");

    // No callbacks registered: the guest reflects the host, not the snapshot.
    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .build()
        .expect("sandbox creation should succeed");
    let result = sandbox
        .execute("print(list_callbacks())")
        .await
        .expect("execution should succeed");
    assert_eq!(result.stdout.trim(), "[]");
}

/// Declaration order does not matter: the snapshot and the host both present
/// callbacks sorted by name.
#[tokio::test]
async fn preinit_baked_callbacks_ignore_registration_order() {
    let stdlib = get_stdlib_path();
    let preinit_bytes = preinit_with_callbacks(&stdlib, &[&Ping, &Echo]).await;

    let sandbox = Sandbox::builder()
        .with_wasm_bytes(preinit_bytes)
        .with_python_stdlib(&stdlib)
        .with_callback(Echo)
        .with_callback(Ping)
        .build()
        .expect("sandbox creation should succeed");
    let result = sandbox
        .execute(
            "print([c['name'] for c in list_callbacks()], await ping(), (await echo(x=1))['x'])",
        )
        .await
        .expect("execution should succeed");
    assert_eq!(result.stdout.trim(), "['echo', 'ping'] pong 1");
}
