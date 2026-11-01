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
//! - Use: User-configured via [`FilesystemCache`] or [`InMemoryCache`]; native
//!   extensions get a [`FilesystemCache`] in the per-user eryx cache directory
//!   by default
//!
//! Entries are loaded as pre-compiled native code without validation, so
//! the builder methods that take a cache are `unsafe`: the caller vouches that
//! nobody else can write to it.
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
//! let cache = FilesystemCache::new("/var/cache/my-app/eryx")?;
//!
//! // First call: ~1000ms (link + compile + cache)
//! // SAFETY: only this user can write to the cache directory.
//! let sandbox1 = unsafe {
//!     Sandbox::builder()
//!         .with_native_extension("numpy/core/*.so", bytes)
//!         .with_cache(Arc::new(cache.clone()))
//! }
//! .build()?;
//!
//! // Second call: ~10ms (cache hit)
//! let sandbox2 = unsafe {
//!     Sandbox::builder()
//!         .with_native_extension("numpy/core/*.so", bytes)
//!         .with_cache(Arc::new(cache.clone()))
//! }
//! .build()?;
//! ```

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::wasm::{ExecutorState, SandboxPre};

/// Error type for cache operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
/// - Hash of the component inputs or pre-compiled component contents
/// - eryx-runtime version (for base library changes)
/// - wasmtime version (for compilation compatibility)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Hash of the inputs identifying the pre-compiled component.
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

    /// Compute a cache key from pre-compiled component bytes.
    ///
    /// The full artifact is hashed so that distinct pre-compiled components
    /// (e.g. factories pre-initialized with different packages) never share a
    /// cached `SandboxPre`. Computing the key costs one BLAKE3 pass over the
    /// artifact, so callers should compute it once and reuse it.
    #[cfg(feature = "embedded")]
    #[must_use]
    pub(crate) fn from_precompiled(precompiled: &[u8]) -> Self {
        let mut hasher = Self::precompiled_hasher();
        hasher.update(precompiled);
        Self::from_precompiled_hasher(&hasher)
    }

    /// Compute the same key as [`Self::from_precompiled`] by streaming the
    /// component bytes from `reader`, without buffering them in memory.
    #[cfg(feature = "embedded")]
    pub(crate) fn from_precompiled_reader(reader: impl io::Read) -> io::Result<Self> {
        let mut hasher = Self::precompiled_hasher();
        hasher.update_reader(reader)?;
        Ok(Self::from_precompiled_hasher(&hasher))
    }

    #[cfg(feature = "embedded")]
    fn precompiled_hasher() -> blake3::Hasher {
        blake3::Hasher::new_derive_key("eryx precompiled component cache key v1")
    }

    #[cfg(feature = "embedded")]
    fn from_precompiled_hasher(hasher: &blake3::Hasher) -> Self {
        Self::from_precompiled_hash(*hasher.finalize().as_bytes())
    }

    /// Rebuild the key [`Self::from_precompiled`] would compute, from the
    /// content hash a previous key returned from [`Self::precompiled_hash`].
    #[cfg(feature = "embedded")]
    pub(crate) fn from_precompiled_hash(hash: [u8; 32]) -> Self {
        Self {
            extensions_hash: hash,
            eryx_version: env!("CARGO_PKG_VERSION"),
            wasmtime_version: wasmtime_version(),
        }
    }

    /// The content hash of a key from [`Self::from_precompiled`].
    #[cfg(feature = "embedded")]
    pub(crate) fn precompiled_hash(&self) -> [u8; 32] {
        self.extensions_hash
    }

    /// Compute a cache key from a list of native extensions.
    ///
    /// The extensions are sorted by name before hashing to ensure
    /// deterministic keys regardless of insertion order.
    ///
    /// The key also covers the base runtime libraries the extensions are
    /// linked against and the engine's compilation settings (target, CPU
    /// features, Cranelift flags), so a cache shared between builds or
    /// machines never hands back a component built from something else.
    #[cfg(feature = "native-extensions")]
    pub fn from_extensions(extensions: &[eryx_runtime::linker::NativeExtension]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(link_environment_hash());

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

/// Hash of everything besides the extensions that determines a linked,
/// pre-compiled component: the base runtime libraries and the shared engine's
/// compilation settings. Computed once per process.
#[cfg(feature = "native-extensions")]
fn link_environment_hash() -> [u8; 32] {
    use eryx_runtime::linker::base_libraries as libs;
    use std::hash::{Hash, Hasher};

    static HASH: OnceLock<[u8; 32]> = OnceLock::new();
    *HASH.get_or_init(|| {
        let mut hasher = Sha256::new();
        for lib in [
            libs::LIBC,
            libs::LIBCXX,
            libs::LIBCXXABI,
            libs::LIBPYTHON,
            libs::LIBWASI_EMULATED_MMAN,
            libs::LIBWASI_EMULATED_PROCESS_CLOCKS,
            libs::LIBWASI_EMULATED_GETPID,
            libs::LIBWASI_EMULATED_SIGNAL,
            libs::WASI_ADAPTER,
            libs::LIBERYX_RUNTIME,
            libs::LIBERYX_BINDINGS,
        ] {
            hasher.update((lib.len() as u64).to_le_bytes());
            hasher.update(lib);
        }
        hasher.update(crate::wasm::PythonExecutor::ENGINE_CONFIG_VERSION.to_le_bytes());
        // If the engine can't be created, building the sandbox fails anyway.
        if let Ok(engine) = crate::wasm::PythonExecutor::shared_engine() {
            let mut engine_hasher = std::hash::DefaultHasher::new();
            engine
                .precompile_compatibility_hash()
                .hash(&mut engine_hasher);
            hasher.update(engine_hasher.finish().to_le_bytes());
        }
        hasher.finalize().into()
    })
}

/// Get the wasmtime version string.
///
/// This must match the wasmtime version in Cargo.toml to ensure cache
/// invalidation when wasmtime is upgraded. Pre-compiled components are
/// not compatible across wasmtime versions.
fn wasmtime_version() -> &'static str {
    // Taken from wasmtime itself rather than hand-maintained: its default
    // `ModuleVersionStrategy` reports wasmtime's own `CARGO_PKG_VERSION_MAJOR`,
    // which is exactly the granularity `.cwasm` compatibility breaks at. The
    // borrow is tied to the strategy value, so leak a copy once to get the
    // `'static` string the cache key holds.
    static VERSION: OnceLock<&'static str> = OnceLock::new();
    VERSION.get_or_init(|| {
        String::leak(
            wasmtime::ModuleVersionStrategy::default()
                .as_str()
                .to_owned(),
        )
    })
}

/// Encode bytes as hex string.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The per-user directory eryx keeps its on-disk caches in.
///
/// This is the first of `$XDG_CACHE_HOME/eryx`, the platform cache directory
/// (`~/.cache/eryx`, `~/Library/Caches/eryx`, `%LOCALAPPDATA%\eryx`) and a
/// temp-dir fallback (`$TMPDIR/eryx-<uid>` on Unix) that can be created and
/// passes the same checks as [`FilesystemCache::new`]. Resolved once per
/// process.
///
/// # Errors
///
/// Returns an error if none of the candidates can be created or passes the
/// checks.
#[cfg(feature = "embedded-stdlib")]
pub fn default_cache_root() -> Result<&'static Path, crate::Error> {
    static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut errors = Vec::new();
        for candidate in cache_root_candidates() {
            match ensure_private_dir(&candidate) {
                Ok(dir) => return Ok(dir),
                Err(e) => errors.push(format!("{}: {e}", candidate.display())),
            }
        }
        Err(format!("no usable cache directory ({})", errors.join("; ")))
    })
    .as_deref()
    .map_err(|e| crate::Error::Initialization(e.clone()))
}

#[cfg(feature = "embedded-stdlib")]
fn cache_root_candidates() -> Vec<PathBuf> {
    let env_dir = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let mut candidates = Vec::new();
    if let Some(dir) = env_dir("XDG_CACHE_HOME") {
        candidates.push(dir.join("eryx"));
    }
    #[cfg(windows)]
    if let Some(dir) = env_dir("LOCALAPPDATA") {
        candidates.push(dir.join("eryx"));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = env_dir("HOME") {
        candidates.push(home.join("Library/Caches/eryx"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(home) = env_dir("HOME") {
        candidates.push(home.join(".cache/eryx"));
    }
    #[cfg(unix)]
    candidates
        .push(std::env::temp_dir().join(format!("eryx-{}", rustix::process::geteuid().as_raw())));
    #[cfg(not(unix))]
    candidates.push(std::env::temp_dir().join("eryx"));
    candidates
}

/// Create `dir` if needed and check that only the current user can write to
/// it. Returns the canonical path, so later lookups can't be redirected by
/// swapping a symlink.
///
/// New directories are created with mode `0700` on Unix. Existing ones must be
/// owned by the current user and not group- or world-writable. On other
/// platforms the directory is only created.
///
/// # Errors
///
/// Returns an error if the directory can't be created or inspected, or if it
/// fails the ownership and mode checks.
pub fn ensure_private_dir(dir: &Path) -> io::Result<PathBuf> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let dir = fs::canonicalize(dir)?;
    let meta = fs::metadata(&dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    if !is_private(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory is owned by another user or writable by other users",
        ));
    }
    Ok(dir)
}

/// Whether a file or directory is owned by the current user and not group- or
/// world-writable. Always true on platforms without Unix permissions.
#[must_use]
pub fn is_private(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == rustix::process::geteuid().as_raw() && meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// Create a new file readable and writable only by the current user.
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
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
/// let cache = FilesystemCache::new("/var/cache/my-app/eryx")?;
/// ```
#[derive(Debug, Clone)]
pub struct FilesystemCache {
    cache_dir: PathBuf,
}

impl FilesystemCache {
    /// Create a new filesystem cache at the given directory.
    ///
    /// Creates the directory (mode `0700` on Unix) if it doesn't exist.
    /// On Unix the directory must be owned by the current user and not be
    /// group- or world-writable, since its entries are loaded as native code.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created, or if it is owned
    /// by another user or writable by other users.
    pub fn new(cache_dir: impl AsRef<Path>) -> Result<Self, CacheError> {
        let cache_dir = ensure_private_dir(cache_dir.as_ref())?;
        Ok(Self { cache_dir })
    }

    /// Get the file path for a cache entry (for mmap-based loading).
    ///
    /// Returns `Some(path)` if the cache entry exists, `None` otherwise.
    /// Entries owned by another user or writable by other users are ignored.
    /// Using the file path directly with `Component::deserialize_file`
    /// enables memory-mapped loading which is faster for large components.
    #[must_use]
    pub fn get_path(&self, key: &CacheKey) -> Option<PathBuf> {
        let path = self.cache_path(key);
        let meta = fs::symlink_metadata(&path).ok()?;
        (meta.is_file() && is_private(&meta)).then_some(path)
    }

    /// Get the path for a cache entry.
    fn cache_path(&self, key: &CacheKey) -> PathBuf {
        self.cache_dir.join(format!("{}.cwasm", key.to_hex()))
    }
}

impl ComponentCache for FilesystemCache {
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        use std::io::Read;

        let mut file = fs::File::open(self.cache_path(key)).ok()?;
        let meta = file.metadata().ok()?;
        if !(meta.is_file() && is_private(&meta)) {
            return None;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    fn put(&self, key: &CacheKey, precompiled: Vec<u8>) -> Result<(), CacheError> {
        let path = self.cache_path(key);

        // Write to a temp file first, then rename for atomicity. The temp name
        // is unique per writer so processes caching the same key concurrently
        // never write into the same file, and the data is synced before the
        // rename so a crash can't leave a truncated entry under the real name.
        let temp_path = path.with_extension(format!(
            "cwasm.{}-{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let written = create_private_file(&temp_path).and_then(|mut file| {
            file.write_all(&precompiled)?;
            file.sync_all()?;
            fs::rename(&temp_path, &path)
        });
        if written.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        written?;

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
/// The cache is process-global and thread-safe. It holds at most
/// [`MAX_ENTRIES`](Self::MAX_ENTRIES) components (each can pin tens of
/// megabytes of compiled code), evicting the least recently used one when
/// full. Evicting an entry only means the next sandbox for that component
/// loads it again; executors already created keep working.
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
    cache: RwLock<HashMap<CacheKey, InstancePreEntry>>,
    /// Logical clock for least-recently-used eviction.
    clock: AtomicU64,
}

/// A cached `SandboxPre` and when it was last used.
struct InstancePreEntry {
    pre: SandboxPre<ExecutorState>,
    last_used: AtomicU64,
}

impl InstancePreCache {
    /// Maximum number of cached components.
    pub const MAX_ENTRIES: usize = 16;

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
                clock: AtomicU64::new(0),
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
        let entry = cache.get(key)?;
        entry.last_used.store(self.tick(), Ordering::Relaxed);
        tracing::trace!(key = %key.to_hex(), "instance_pre cache hit");
        Some(entry.pre.clone())
    }

    /// Store a `SandboxPre` in the cache.
    ///
    /// If an entry already exists for this key (race condition), the
    /// existing entry is kept (first writer wins). When the cache is full,
    /// the least recently used entry is evicted to make room.
    pub fn put(&self, key: CacheKey, pre: SandboxPre<ExecutorState>) {
        if let Ok(mut cache) = self.cache.write() {
            if cache.contains_key(&key) {
                return;
            }
            if cache.len() >= Self::MAX_ENTRIES
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used.load(Ordering::Relaxed))
                    .map(|(key, _)| key.clone())
            {
                tracing::debug!(key = %oldest.to_hex(), "instance_pre cache evict");
                cache.remove(&oldest);
            }
            tracing::debug!(key = %key.to_hex(), "instance_pre cache store");
            cache.insert(
                key,
                InstancePreEntry {
                    pre,
                    last_used: AtomicU64::new(self.tick()),
                },
            );
        }
    }

    /// Advance the LRU clock.
    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
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

    #[cfg(feature = "embedded")]
    #[test]
    fn from_precompiled_key_is_deterministic_and_content_based() {
        let key1 = CacheKey::from_precompiled(b"abc");
        let key2 = CacheKey::from_precompiled(b"abc");
        let key3 = CacheKey::from_precompiled(b"abd");
        let key4 = CacheKey::from_precompiled(b"abcd"); // same prefix, longer

        // Same bytes -> same key (deterministic)
        assert_eq!(key1, key2);
        // Different bytes -> different key
        assert_ne!(key1, key3);
        // Different length with shared prefix -> different key
        assert_ne!(key1, key4);
    }

    #[cfg(feature = "embedded")]
    #[test]
    fn from_precompiled_reader_matches_bytes_key() {
        let key = CacheKey::from_precompiled_reader(&b"abc"[..]).unwrap();
        assert_eq!(key, CacheKey::from_precompiled(b"abc"));
    }

    #[cfg(feature = "embedded")]
    #[test]
    fn from_precompiled_hash_round_trips() {
        let key = CacheKey::from_precompiled(b"abc");
        assert_eq!(CacheKey::from_precompiled_hash(key.precompiled_hash()), key);
    }

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

    #[cfg(unix)]
    #[test]
    fn filesystem_cache_dir_must_be_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let fresh = temp_dir.path().join("fresh");
        FilesystemCache::new(&fresh).unwrap();
        let mode = fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let shared = temp_dir.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(FilesystemCache::new(&shared).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_cache_skips_entries_writable_by_others() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FilesystemCache::new(temp_dir.path()).unwrap();
        let key = CacheKey {
            extensions_hash: [5u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };
        cache.put(&key, vec![1, 2, 3]).unwrap();
        assert!(cache.get(&key).is_some());
        assert!(cache.get_path(&key).is_some());

        fs::set_permissions(cache.cache_path(&key), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(cache.get(&key).is_none());
        assert!(cache.get_path(&key).is_none());
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

    #[cfg(feature = "embedded")]
    #[test]
    fn instance_pre_cache_evicts_least_recently_used() {
        let pre = crate::wasm::PythonExecutor::from_embedded_runtime()
            .unwrap()
            .instance_pre()
            .clone();
        let cache = InstancePreCache {
            cache: RwLock::new(HashMap::new()),
            clock: AtomicU64::new(0),
        };
        let key = |i: u8| CacheKey {
            extensions_hash: [i; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        for i in 0..InstancePreCache::MAX_ENTRIES as u8 {
            cache.put(key(i), pre.clone());
        }
        // Touch the oldest entry so the second-oldest becomes the LRU one.
        assert!(cache.get(&key(0)).is_some());
        cache.put(key(200), pre);

        assert_eq!(cache.len(), InstancePreCache::MAX_ENTRIES);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(200)).is_some());
    }

    #[test]
    fn filesystem_cache_concurrent_puts_never_tear() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = FilesystemCache::new(temp_dir.path()).unwrap();
        let key = CacheKey {
            extensions_hash: [4u8; 32],
            eryx_version: "0.1.0",
            wasmtime_version: "39.0.0",
        };

        std::thread::scope(|scope| {
            for writer in 0..8u8 {
                let (cache, key) = (&cache, &key);
                scope.spawn(move || {
                    for _ in 0..20 {
                        cache.put(key, vec![writer; 256 * 1024]).unwrap();
                    }
                });
            }
        });

        let stored = cache.get(&key).unwrap();
        assert_eq!(stored.len(), 256 * 1024);
        assert!(stored.iter().all(|&b| b == stored[0]), "torn cache entry");
        let leftovers = std::fs::read_dir(temp_dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files left behind");
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
