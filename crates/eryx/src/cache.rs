//! Component caching for faster sandbox creation.
//!
//! This module provides caching of pre-compiled WASM components to avoid
//! expensive linking and JIT compilation on repeated sandbox creations with
//! the same native extensions.
//!
//! # Cache Levels
//!
//! There are two levels of caching:
//!
//! 1. **Level 1**: Cache linked component bytes (.wasm format)
//!    - Saves linking time (~1000ms)
//!    - Still requires JIT compilation (~500ms)
//!
//! 2. **Level 2**: Cache pre-compiled component bytes (.cwasm format)
//!    - Saves both linking AND compilation
//!    - 100x speedup on cache hit (~10ms)
//!
//! This module implements Level 2 caching for maximum performance.
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::{Sandbox, cache::FilesystemCache};
//!
//! let cache = FilesystemCache::new("/tmp/eryx-cache")?;
//!
//! // First call: ~1000ms (link + compile + cache)
//! let sandbox1 = Sandbox::builder()
//!     .with_native_extension("numpy/core/*.so", bytes)
//!     .with_cache(cache.clone())
//!     .build()?;
//!
//! // Second call: ~10ms (cache hit)
//! let sandbox2 = Sandbox::builder()
//!     .with_native_extension("numpy/core/*.so", bytes)
//!     .with_cache(cache.clone())
//!     .build()?;
//! ```

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Error type for cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// I/O error when reading or writing cache files.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Cache entry is corrupted or invalid.
    #[error("Cache entry corrupted: {0}")]
    Corrupted(String),
}

/// Trait for component caching implementations.
///
/// Implementations of this trait can cache pre-compiled WASM components
/// to avoid expensive linking and JIT compilation on repeated sandbox
/// creations.
pub trait ComponentCache: Send + Sync {
    /// Get pre-compiled component bytes for the given cache key.
    ///
    /// Returns `None` if the key is not in the cache.
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>>;

    /// Store pre-compiled component bytes with the given cache key.
    ///
    /// Returns `Ok(())` on success, or an error if the cache operation fails.
    fn put(&self, key: &CacheKey, precompiled: Vec<u8>) -> Result<(), CacheError>;
}

/// Cache key for identifying pre-compiled components.
///
/// The key includes:
/// - Hash of all native extension contents
/// - eryx-runtime version (for base library changes)
/// - wasmtime version (for compilation compatibility)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Hash of native extensions (sorted by name for determinism).
    pub extensions_hash: [u8; 32],
    /// Version of eryx-runtime crate.
    pub eryx_version: &'static str,
    /// Version of wasmtime crate.
    pub wasmtime_version: &'static str,
}

impl CacheKey {
    /// Compute a cache key from a list of native extensions.
    ///
    /// The extensions are sorted by name before hashing to ensure
    /// deterministic keys regardless of insertion order.
    #[cfg(feature = "native-extensions")]
    pub fn from_extensions(extensions: &[eryx_runtime::linker::NativeExtension]) -> Self {
        let mut hasher = Sha256::new();

        // Sort by name for determinism
        let mut sorted: Vec<_> = extensions.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for ext in sorted {
            hasher.update(ext.name.as_bytes());
            hasher.update((ext.bytes.len() as u64).to_le_bytes());
            hasher.update(&ext.bytes);
        }

        let extensions_hash: [u8; 32] = hasher.finalize().into();

        Self {
            extensions_hash,
            eryx_version: env!("CARGO_PKG_VERSION"),
            wasmtime_version: wasmtime_version(),
        }
    }

    /// Get a hex string representation of the full cache key.
    ///
    /// This is used as a filename in filesystem caches.
    #[must_use]
    pub fn to_hex(&self) -> String {
        // Include version info in the hash to avoid collisions
        let mut hasher = Sha256::new();
        hasher.update(self.extensions_hash);
        hasher.update(self.eryx_version.as_bytes());
        hasher.update(self.wasmtime_version.as_bytes());
        let full_hash: [u8; 32] = hasher.finalize().into();

        hex_encode(&full_hash)
    }
}

/// Get the wasmtime version string.
fn wasmtime_version() -> &'static str {
    // Get from wasmtime crate version
    "39.0.0" // TODO: Use actual wasmtime version from Cargo
}

/// Encode bytes as hex string.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Filesystem-based component cache.
///
/// Caches pre-compiled components as `.cwasm` files in a directory.
/// Files are named by the hex-encoded cache key hash.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::cache::FilesystemCache;
///
/// let cache = FilesystemCache::new("/tmp/eryx-cache")?;
/// ```
#[derive(Debug, Clone)]
pub struct FilesystemCache {
    cache_dir: PathBuf,
}

impl FilesystemCache {
    /// Create a new filesystem cache at the given directory.
    ///
    /// Creates the directory if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn new(cache_dir: impl AsRef<Path>) -> Result<Self, CacheError> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    /// Get the file path for a cache entry (for mmap-based loading).
    ///
    /// Returns `Some(path)` if the cache entry exists, `None` otherwise.
    /// Using the file path directly with `Component::deserialize_file`
    /// enables memory-mapped loading which is faster for large components.
    #[must_use]
    pub fn get_path(&self, key: &CacheKey) -> Option<PathBuf> {
        let path = self.cache_path(key);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Get the path for a cache entry.
    fn cache_path(&self, key: &CacheKey) -> PathBuf {
        self.cache_dir.join(format!("{}.cwasm", key.to_hex()))
    }
}

impl ComponentCache for FilesystemCache {
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let path = self.cache_path(key);
        fs::read(&path).ok()
    }

    fn put(&self, key: &CacheKey, precompiled: Vec<u8>) -> Result<(), CacheError> {
        let path = self.cache_path(key);

        // Write to a temp file first, then rename for atomicity
        let temp_path = path.with_extension("cwasm.tmp");
        fs::write(&temp_path, &precompiled)?;
        fs::rename(&temp_path, &path)?;

        Ok(())
    }
}

/// In-memory component cache.
///
/// Caches pre-compiled components in memory. Useful for testing or
/// applications that create many sandboxes with the same extensions
/// within a single process.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::cache::InMemoryCache;
///
/// let cache = InMemoryCache::new();
/// ```
#[derive(Debug, Clone, Default)]
pub struct InMemoryCache {
    cache: Arc<Mutex<HashMap<[u8; 32], Vec<u8>>>>,
}

impl InMemoryCache {
    /// Create a new in-memory cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ComponentCache for InMemoryCache {
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let cache = self.cache.lock().ok()?;
        // Use the full key hash (including versions) for lookup
        let mut hasher = Sha256::new();
        hasher.update(key.extensions_hash);
        hasher.update(key.eryx_version.as_bytes());
        hasher.update(key.wasmtime_version.as_bytes());
        let full_hash: [u8; 32] = hasher.finalize().into();

        cache.get(&full_hash).cloned()
    }

    fn put(&self, key: &CacheKey, precompiled: Vec<u8>) -> Result<(), CacheError> {
        let mut cache = self.cache.lock().map_err(|e| {
            CacheError::Corrupted(format!("Cache lock poisoned: {e}"))
        })?;

        let mut hasher = Sha256::new();
        hasher.update(key.extensions_hash);
        hasher.update(key.eryx_version.as_bytes());
        hasher.update(key.wasmtime_version.as_bytes());
        let full_hash: [u8; 32] = hasher.finalize().into();

        cache.insert(full_hash, precompiled);
        Ok(())
    }
}

/// A cache implementation that never caches anything.
///
/// Useful for disabling caching explicitly or in tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCache;

impl ComponentCache for NoCache {
    fn get(&self, _key: &CacheKey) -> Option<Vec<u8>> {
        None
    }

    fn put(&self, _key: &CacheKey, _precompiled: Vec<u8>) -> Result<(), CacheError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_to_hex_is_deterministic() {
        let key = CacheKey {
            extensions_hash: [0u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        let hex1 = key.to_hex();
        let hex2 = key.to_hex();
        assert_eq!(hex1, hex2);
        assert_eq!(hex1.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn cache_key_different_versions_produce_different_hex() {
        let key1 = CacheKey {
            extensions_hash: [0u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        let key2 = CacheKey {
            extensions_hash: [0u8; 32],
            eryx_version: "0.2.0",
            wasmtime_version: "39.0.0",
        };

        assert_ne!(key1.to_hex(), key2.to_hex());
    }

    #[test]
    fn in_memory_cache_stores_and_retrieves() {
        let cache = InMemoryCache::new();
        let key = CacheKey {
            extensions_hash: [1u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        // Initially empty
        assert!(cache.get(&key).is_none());

        // Store something
        let data = vec![1, 2, 3, 4];
        cache.put(&key, data.clone()).unwrap();

        // Retrieve it
        let retrieved = cache.get(&key);
        assert_eq!(retrieved, Some(data));
    }

    #[test]
    fn no_cache_never_stores() {
        let cache = NoCache;
        let key = CacheKey {
            extensions_hash: [2u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        // Store something
        let data = vec![5, 6, 7, 8];
        cache.put(&key, data).unwrap();

        // Still empty
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn filesystem_cache_creates_directory() {
        let temp_dir = std::env::temp_dir().join("eryx-cache-test");
        let _ = std::fs::remove_dir_all(&temp_dir); // Clean up any previous run

        let cache = FilesystemCache::new(&temp_dir).unwrap();
        assert!(temp_dir.exists());

        // Clean up
        drop(cache);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn filesystem_cache_stores_and_retrieves() {
        let temp_dir = std::env::temp_dir().join("eryx-cache-test-2");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let cache = FilesystemCache::new(&temp_dir).unwrap();
        let key = CacheKey {
            extensions_hash: [3u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        // Initially empty
        assert!(cache.get(&key).is_none());

        // Store something
        let data = vec![10, 20, 30, 40];
        cache.put(&key, data.clone()).unwrap();

        // Retrieve it
        let retrieved = cache.get(&key);
        assert_eq!(retrieved, Some(data));

        // Verify file exists
        let expected_path = cache.cache_path(&key);
        assert!(expected_path.exists());

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
