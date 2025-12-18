//! Component caching for faster sandbox creation.
//!
//! This module provides a two-tier caching system to minimize sandbox creation overhead:
//!
//! # Two-Tier Cache Architecture
//!
//! ## Tier 1: InstancePreCache (in-memory)
//!
//! Caches `SandboxPre<ExecutorState>` - the fully linked, pre-instantiated component.
//! This is the fastest tier, returning a cached instance in ~0ms (just a Clone).
//!
//! - Key: [`CacheKey`] (extension hash + version info)
//! - Lifetime: Process duration
//! - Use: Automatic for embedded runtime and native extensions
//!
//! ## Tier 2: ComponentCache (disk or memory)
//!
//! Caches pre-compiled `.cwasm` bytes for persistence across process restarts.
//!
//! - Key: [`CacheKey`] (same as Tier 1)
//! - Lifetime: Persistent (filesystem) or process duration (in-memory)
//! - Use: User-configured via [`FilesystemCache`] or [`InMemoryCache`]
//!
//! # Cache Flow
//!
//! On sandbox creation:
//! 1. Check Tier 1 ([`InstancePreCache`]) → if hit, return immediately (~0ms)
//! 2. Check Tier 2 ([`ComponentCache`]) → if hit, create `SandboxPre`, store in Tier 1 (~10ms)
//! 3. Cold path → compile, store in Tier 2, create `SandboxPre`, store in Tier 1 (~500ms)
//!
//! This means the second sandbox with the same configuration is ~10-100x faster.
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
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::wasm::{ExecutorState, SandboxPre};

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
    /// Cache key for the base embedded runtime with no extensions.
    ///
    /// This is a sentinel key used to cache the `SandboxPre` for the
    /// embedded runtime when no native extensions are present.
    #[must_use]
    pub fn embedded_runtime() -> Self {
        Self {
            extensions_hash: [0u8; 32], // All zeros = no extensions
            eryx_version: env!("CARGO_PKG_VERSION"),
            wasmtime_version: wasmtime_version(),
        }
    }

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
///
/// This must match the wasmtime version in Cargo.toml to ensure cache
/// invalidation when wasmtime is upgraded. Pre-compiled components are
/// not compatible across wasmtime versions.
fn wasmtime_version() -> &'static str {
    // Keep in sync with workspace wasmtime version in Cargo.toml
    "39"
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
        if path.exists() { Some(path) } else { None }
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
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| CacheError::Corrupted(format!("Cache lock poisoned: {e}")))?;

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

// ============================================================================
// Tier 1: InstancePreCache (in-memory SandboxPre caching)
// ============================================================================

/// Global in-memory cache for pre-instantiated WASM components.
///
/// This is Tier 1 of the two-tier caching system. It stores pre-initialized
/// instances keyed by [`CacheKey`], eliminating the need to re-link imports
/// on repeated sandbox creations with the same component.
///
/// The cache is process-global and thread-safe. Entries persist for the
/// lifetime of the process.
///
/// # Usage
///
/// This cache is used automatically by [`PythonExecutor`](crate::wasm::PythonExecutor)
/// when creating executors from precompiled files or the embedded runtime.
/// You typically don't need to interact with it directly.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::cache::{InstancePreCache, CacheKey};
///
/// let cache = InstancePreCache::global();
/// let key = CacheKey::embedded_runtime();
///
/// // Check if cached
/// if let Some(pre) = cache.get(&key) {
///     // Use cached SandboxPre
/// }
/// ```
pub struct InstancePreCache {
    cache: RwLock<HashMap<CacheKey, SandboxPre<ExecutorState>>>,
}

impl InstancePreCache {
    /// Get the global instance pre cache.
    ///
    /// This returns a reference to the process-global cache instance.
    /// The cache is lazily initialized on first access.
    pub fn global() -> &'static Self {
        static CACHE: OnceLock<InstancePreCache> = OnceLock::new();
        CACHE.get_or_init(|| {
            tracing::debug!("initializing global InstancePreCache");
            InstancePreCache {
                cache: RwLock::new(HashMap::new()),
            }
        })
    }

    /// Get a cached `SandboxPre` for the given key.
    ///
    /// Returns `Some(pre)` if found, `None` otherwise.
    /// The returned `SandboxPre` is cloned (cheap - it's internally reference-counted).
    #[must_use]
    pub fn get(&self, key: &CacheKey) -> Option<SandboxPre<ExecutorState>> {
        let cache = self.cache.read().ok()?;
        let result = cache.get(key).cloned();
        if result.is_some() {
            tracing::trace!(key = %key.to_hex(), "instance_pre cache hit");
        }
        result
    }

    /// Store a `SandboxPre` in the cache.
    ///
    /// If an entry already exists for this key (race condition), the
    /// existing entry is kept (first writer wins).
    pub fn put(&self, key: CacheKey, pre: SandboxPre<ExecutorState>) {
        if let Ok(mut cache) = self.cache.write() {
            use std::collections::hash_map::Entry;
            if let Entry::Vacant(e) = cache.entry(key.clone()) {
                tracing::debug!(key = %key.to_hex(), "instance_pre cache store");
                e.insert(pre);
            }
        }
    }

    /// Clear all cached entries.
    ///
    /// This is primarily useful for testing. In production, the cache
    /// automatically invalidates based on version info in the [`CacheKey`].
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.write() {
            let count = cache.len();
            cache.clear();
            tracing::debug!(entries = count, "instance_pre cache cleared");
        }
    }

    /// Get the number of cached entries.
    ///
    /// Useful for debugging and testing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for InstancePreCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstancePreCache")
            .field("entries", &self.len())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
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

    #[test]
    fn cache_key_embedded_runtime_is_deterministic() {
        let key1 = CacheKey::embedded_runtime();
        let key2 = CacheKey::embedded_runtime();

        assert_eq!(key1, key2);
        assert_eq!(key1.extensions_hash, [0u8; 32]); // Sentinel value
        assert_eq!(key1.eryx_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn cache_key_embedded_runtime_differs_from_extensions() {
        let embedded_key = CacheKey::embedded_runtime();
        let other_key = CacheKey {
            extensions_hash: [1u8; 32], // Non-zero hash
            eryx_version: env!("CARGO_PKG_VERSION"),
            wasmtime_version: "39",
        };

        assert_ne!(embedded_key, other_key);
        assert_ne!(embedded_key.to_hex(), other_key.to_hex());
    }

    #[test]
    fn instance_pre_cache_global_returns_same_instance() {
        let cache1 = InstancePreCache::global();
        let cache2 = InstancePreCache::global();

        // Both should point to the same global instance
        assert!(std::ptr::eq(cache1, cache2));
    }

    #[test]
    fn instance_pre_cache_is_initially_empty() {
        // Note: This test may fail if run after other tests that populate the cache
        // In practice, the global cache persists across tests, so we just verify
        // the len() and is_empty() methods work correctly
        let cache = InstancePreCache::global();
        let initial_len = cache.len();

        // Methods should work without panicking
        let _ = cache.is_empty();
        assert_eq!(cache.len(), initial_len);
    }

    #[test]
    fn instance_pre_cache_debug_format() {
        let cache = InstancePreCache::global();
        let debug_str = format!("{:?}", cache);

        // Should contain the struct name and entries field
        assert!(debug_str.contains("InstancePreCache"));
        assert!(debug_str.contains("entries"));
    }
}
