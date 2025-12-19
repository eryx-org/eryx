//! Integration tests for embedded runtime, embedded stdlib, packages, and caching.
//!
//! These tests verify the "zero-config" API works correctly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::Sandbox;

// =============================================================================
// Embedded Runtime Tests
// =============================================================================

/// Test that sandbox creation works with Sandbox::embedded().
#[tokio::test]
#[cfg(feature = "embedded")]
async fn embedded_runtime_is_automatic() {
    let sandbox = Sandbox::embedded()
        .build()
        .expect("Sandbox::embedded() should build successfully");

    let result = sandbox
        .execute("print('hello')")
        .await
        .expect("execution should work");
    assert!(result.stdout.contains("hello"));
}

/// Test that multiple sandboxes can be created quickly with embedded runtime.
#[tokio::test]
#[cfg(feature = "embedded")]
async fn embedded_runtime_multiple_sandboxes() {
    // Create 5 sandboxes - should be fast after first extraction
    for i in 0..5 {
        let sandbox = Sandbox::embedded()
            .build()
            .expect("sandbox creation should work");
        let result = sandbox
            .execute(&format!("print('sandbox {i}')"))
            .await
            .expect("execution should work");
        assert!(result.stdout.contains(&format!("sandbox {i}")));
    }
}

// =============================================================================
// Embedded Stdlib Tests
// =============================================================================

/// Test that embedded stdlib is used automatically when no explicit path is set.
#[tokio::test]
#[cfg(feature = "embedded")]
async fn embedded_stdlib_is_automatic() {
    let sandbox = Sandbox::embedded()
        .build()
        .expect("Sandbox::embedded() includes stdlib");

    // Test that stdlib modules are available
    let result = sandbox
        .execute("import json; print(json.dumps({'a': 1}))")
        .await
        .expect("json module should be available");
    assert!(result.stdout.contains(r#"{"a": 1}"#));
}

/// Test that various stdlib modules work with embedded stdlib.
#[tokio::test]
#[cfg(feature = "embedded")]
async fn embedded_stdlib_modules() {
    let sandbox = Sandbox::embedded().build().unwrap();

    // Test multiple stdlib modules
    let code = r#"
import json
import base64
import hashlib
import re

# json
print(json.dumps([1, 2, 3]))

# base64
print(base64.b64encode(b'hello').decode())

# hashlib
print(hashlib.md5(b'test').hexdigest()[:8])

# re
print(re.match(r'\d+', '123abc').group())
"#;

    let result = sandbox.execute(code).await.unwrap();
    assert!(result.stdout.contains("[1, 2, 3]"));
    assert!(result.stdout.contains("aGVsbG8=")); // base64 of 'hello'
    assert!(result.stdout.contains("098f6bcd")); // md5 of 'test' prefix
    assert!(result.stdout.contains("123"));
}

// =============================================================================
// Package Tests
// =============================================================================

/// Test package format detection.
#[test]
fn package_format_detection() {
    use eryx::package::PackageFormat;
    use std::path::Path;

    assert_eq!(
        PackageFormat::detect(Path::new("requests-2.31.0-py3-none-any.whl")),
        Some(PackageFormat::Wheel)
    );
    assert_eq!(
        PackageFormat::detect(Path::new("numpy-wasi.tar.gz")),
        Some(PackageFormat::TarGz)
    );
    assert_eq!(
        PackageFormat::detect(Path::new("package.tgz")),
        Some(PackageFormat::TarGz)
    );
    assert_eq!(PackageFormat::detect(Path::new("unknown.zip")), None);
    assert_eq!(PackageFormat::detect(Path::new("file.txt")), None);
}

/// Test that with_package returns error for non-existent file.
#[test]
fn package_missing_file_error() {
    let result = Sandbox::builder().with_package("/nonexistent/package.whl");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Failed to open") || err.contains("No such file"));
}

/// Test that with_package returns error for unknown format.
#[test]
fn package_unknown_format_error() {
    // Create a temp file with unknown extension
    let temp_dir = std::env::temp_dir().join("eryx-test-unknown-format");
    let _ = std::fs::create_dir_all(&temp_dir);
    let temp_file = temp_dir.join("package.unknown");
    std::fs::write(&temp_file, "not a package").unwrap();

    let result = Sandbox::builder().with_package(&temp_file);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Cannot detect package format"));

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);
}

// =============================================================================
// Cache Tests
// =============================================================================

/// Test that cache module types are accessible.
#[test]
fn cache_types_exist() {
    use eryx::cache::{CacheError, FilesystemCache, InMemoryCache, NoCache};

    // Just verify types compile
    let _no_cache = NoCache;
    let _in_memory = InMemoryCache::new();

    let temp_dir = std::env::temp_dir().join("eryx-cache-test-types");
    let _fs_cache = FilesystemCache::new(&temp_dir);
    let _ = std::fs::remove_dir_all(&temp_dir);

    // Verify CacheError variants
    let _io_err: CacheError = std::io::Error::new(std::io::ErrorKind::NotFound, "test").into();
}

/// Test FilesystemCache basic operations.
#[test]
fn filesystem_cache_basic() {
    use eryx::cache::{CacheKey, ComponentCache, FilesystemCache};

    let temp_dir = std::env::temp_dir().join("eryx-cache-test-basic");
    let _ = std::fs::remove_dir_all(&temp_dir);

    let cache = FilesystemCache::new(&temp_dir).unwrap();

    let key = CacheKey {
        extensions_hash: [1u8; 32],
        eryx_version: "test",
        wasmtime_version: "test",
    };

    // Initially empty
    assert!(cache.get(&key).is_none());

    // Store and retrieve
    let data = vec![1, 2, 3, 4, 5];
    cache.put(&key, data.clone()).unwrap();
    assert_eq!(cache.get(&key), Some(data));

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Test InMemoryCache basic operations.
#[test]
fn in_memory_cache_basic() {
    use eryx::cache::{CacheKey, ComponentCache, InMemoryCache};

    let cache = InMemoryCache::new();

    let key = CacheKey {
        extensions_hash: [2u8; 32],
        eryx_version: "test",
        wasmtime_version: "test",
    };

    // Initially empty
    assert!(cache.get(&key).is_none());

    // Store and retrieve
    let data = vec![10, 20, 30];
    cache.put(&key, data.clone()).unwrap();
    assert_eq!(cache.get(&key), Some(data));
}

/// Test NoCache never stores anything.
#[test]
fn no_cache_never_stores() {
    use eryx::cache::{CacheKey, ComponentCache, NoCache};

    let cache = NoCache;

    let key = CacheKey {
        extensions_hash: [3u8; 32],
        eryx_version: "test",
        wasmtime_version: "test",
    };

    // Store something
    cache.put(&key, vec![1, 2, 3]).unwrap();

    // Still empty
    assert!(cache.get(&key).is_none());
}

// =============================================================================
// Embedded Resources Tests
// =============================================================================

/// Test that EmbeddedResources extracts to temp directory.
#[test]
#[cfg(feature = "embedded")]
fn embedded_resources_extraction() {
    use eryx::embedded::EmbeddedResources;

    let resources = EmbeddedResources::get().expect("should extract resources");

    assert!(resources.stdlib_path.exists(), "stdlib should be extracted");
    assert!(
        resources.stdlib_path.join("encodings").exists(),
        "encodings module should exist"
    );

    assert!(
        resources.runtime_path.exists(),
        "runtime should be extracted"
    );
    assert!(
        resources
            .runtime_path
            .extension()
            .is_some_and(|e| e == "cwasm"),
        "runtime should be .cwasm file"
    );
}

/// Test that repeated calls to EmbeddedResources::get() return same paths.
#[test]
#[cfg(feature = "embedded")]
fn embedded_resources_cached() {
    use eryx::embedded::EmbeddedResources;

    let resources1 = EmbeddedResources::get().unwrap();
    let resources2 = EmbeddedResources::get().unwrap();

    // Should be the same instance (via OnceLock)
    assert_eq!(resources1.stdlib_path, resources2.stdlib_path);
    assert_eq!(resources1.runtime_path, resources2.runtime_path);
}
