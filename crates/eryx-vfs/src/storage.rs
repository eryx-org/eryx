//! VFS storage trait and implementations.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::runtime::RuntimeFlavor;
use tokio::sync::RwLock;

use crate::error::{VfsError, VfsResult};

/// Metadata for a file or directory.
#[derive(Debug, Clone)]
pub struct Metadata {
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Creation time.
    pub created: SystemTime,
    /// Last modification time.
    pub modified: SystemTime,
    /// Last access time.
    pub accessed: SystemTime,
}

impl Default for Metadata {
    fn default() -> Self {
        let now = SystemTime::now();
        Self {
            is_dir: false,
            size: 0,
            created: now,
            modified: now,
            accessed: now,
        }
    }
}

/// A directory entry returned by listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Name of the entry (not full path).
    pub name: String,
    /// Metadata for the entry.
    pub metadata: Metadata,
}

/// Trait for VFS storage backends.
///
/// Implementations must be thread-safe and support async operations.
/// All paths are absolute paths starting with `/`.
#[async_trait]
pub trait VfsStorage: Send + Sync {
    /// Downcasting hook for storage-specific operations (e.g. capturing an
    /// in-memory snapshot).
    ///
    /// Concrete storages that support downcasting return `Some(self)`; the
    /// default is `None`. [`InMemoryStorage`] returns `Some` so callers can
    /// reach [`InMemoryStorage::snapshot`].
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Read file contents.
    async fn read(&self, path: &str) -> VfsResult<Vec<u8>>;

    /// Read a portion of file contents.
    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>>;

    /// Write file contents (creates or overwrites).
    async fn write(&self, path: &str, data: &[u8]) -> VfsResult<()>;

    /// Write at a specific offset, extending the file if necessary.
    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()>;

    /// Truncate or extend file to the given size.
    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()>;

    /// Delete a file.
    async fn delete(&self, path: &str) -> VfsResult<()>;

    /// Check if a path exists.
    async fn exists(&self, path: &str) -> VfsResult<bool>;

    /// List directory contents.
    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>>;

    /// Get file/directory metadata.
    async fn stat(&self, path: &str) -> VfsResult<Metadata>;

    /// Create a directory.
    async fn mkdir(&self, path: &str) -> VfsResult<()>;

    /// Remove a directory (must be empty).
    async fn rmdir(&self, path: &str) -> VfsResult<()>;

    /// Rename/move a file or directory.
    async fn rename(&self, from: &str, to: &str) -> VfsResult<()>;

    /// Set the access and/or modification time of a file or directory;
    /// `None` leaves that time unchanged.
    ///
    /// The default implementation reports the operation as unsupported.
    async fn set_times(
        &self,
        path: &str,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> VfsResult<()> {
        let _ = (accessed, modified);
        Err(VfsError::Unsupported(format!("set_times: {path}")))
    }

    /// Synchronously create a directory.
    ///
    /// This is useful for setup code that runs before the async runtime
    /// is available. The default implementation returns an error - implementors
    /// should override this if they support sync directory creation.
    ///
    /// Implementations must not deadlock when called from inside a tokio
    /// runtime: [`InMemoryStorage`] waits for a contended lock only on a
    /// multi-thread runtime (or outside any runtime) and otherwise returns an
    /// error.
    fn mkdir_sync(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::Storage(
            "mkdir_sync not implemented for this storage backend".to_string(),
        ))
    }
}

/// Type-erased VFS storage that wraps any [`VfsStorage`] implementation.
///
/// This avoids propagating generic type parameters through the entire
/// executor stack while still allowing different storage backends
/// (e.g., [`InMemoryStorage`], [`ScrubbingStorage`](crate::ScrubbingStorage))
/// to be used interchangeably.
///
/// # Example
///
/// ```rust,ignore
/// use eryx_vfs::{ArcStorage, InMemoryStorage};
/// use std::sync::Arc;
///
/// // Wrap any VfsStorage in a ArcStorage
/// let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()));
/// ```
#[derive(Clone)]
pub struct ArcStorage(Arc<dyn VfsStorage>);

impl ArcStorage {
    /// Wrap any `VfsStorage` implementation in a type-erased `ArcStorage`.
    pub fn new(storage: Arc<dyn VfsStorage>) -> Self {
        Self(storage)
    }
}

impl std::fmt::Debug for ArcStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArcStorage")
            .field(&"<dyn VfsStorage>")
            .finish()
    }
}

#[async_trait]
impl VfsStorage for ArcStorage {
    async fn read(&self, path: &str) -> VfsResult<Vec<u8>> {
        self.0.read(path).await
    }

    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>> {
        self.0.read_at(path, offset, len).await
    }

    async fn write(&self, path: &str, data: &[u8]) -> VfsResult<()> {
        self.0.write(path, data).await
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        self.0.write_at(path, offset, data).await
    }

    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()> {
        self.0.set_size(path, size).await
    }

    async fn delete(&self, path: &str) -> VfsResult<()> {
        self.0.delete(path).await
    }

    async fn exists(&self, path: &str) -> VfsResult<bool> {
        self.0.exists(path).await
    }

    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.0.list(path).await
    }

    async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.0.stat(path).await
    }

    async fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.0.mkdir(path).await
    }

    async fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.0.rmdir(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.0.rename(from, to).await
    }

    async fn set_times(
        &self,
        path: &str,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> VfsResult<()> {
        self.0.set_times(path, accessed, modified).await
    }

    fn mkdir_sync(&self, path: &str) -> VfsResult<()> {
        self.0.mkdir_sync(path)
    }
}

/// Internal state for in-memory storage.
///
/// Combining files and directories into a single struct allows us to use
/// a single `RwLock`, avoiding potential deadlock issues from acquiring
/// multiple locks.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct StorageState {
    /// File contents: path -> data
    files: HashMap<String, FileData>,
    /// Directory markers: set of directory paths
    directories: HashSet<String>,
}

/// Default limit on the total bytes of file content an [`InMemoryStorage`]
/// will hold: 64 MiB.
///
/// File contents live in host memory, and a sandboxed guest chooses both the
/// offsets it writes at and the sizes it truncates to. Without a limit, a
/// one-byte write at a large offset turns into an arbitrarily large host
/// allocation. Override with [`InMemoryStorage::with_max_bytes`].
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// In-memory VFS storage implementation.
///
/// Stores files and directories in memory using `HashMap` and `HashSet`.
/// Thread-safe via a single `RwLock` over the combined state.
///
/// Total file content is capped at [`DEFAULT_MAX_BYTES`] unless changed with
/// [`InMemoryStorage::with_max_bytes`]; operations that would exceed the cap
/// fail with [`VfsError::QuotaExceeded`], which guests see as `ENOSPC`.
#[derive(Debug)]
pub struct InMemoryStorage {
    /// Combined state under a single lock to prevent deadlocks.
    state: RwLock<StorageState>,
    /// Maximum total bytes of file content, across all files.
    max_bytes: u64,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct FileData {
    content: Vec<u8>,
    created: SystemTime,
    modified: SystemTime,
    accessed: SystemTime,
}

/// A clonable, optionally-serializable snapshot of an [`InMemoryStorage`]'s
/// complete contents (all files and directories).
///
/// Produced by [`InMemoryStorage::snapshot`]. Apply it to an existing storage
/// with [`InMemoryStorage::restore`] (in place, preserving the storage's
/// identity so existing `Arc` references stay valid), or create an independent
/// copy with [`InMemoryStorage::from_snapshot`] (for forking a session).
///
/// With the `serde` feature, this derives `Serialize`/`Deserialize` so it can be
/// persisted to bytes.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InMemorySnapshot {
    state: StorageState,
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage with root directory.
    ///
    /// Total file content is capped at [`DEFAULT_MAX_BYTES`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_BYTES)
    }

    /// Create a new empty in-memory storage with an explicit size cap.
    ///
    /// `max_bytes` bounds the total content of all files combined. Because the
    /// contents live in host memory and a sandboxed guest picks its own write
    /// offsets, this is a limit on how much memory the guest can make the host
    /// allocate - keep it well below what the host can afford to lose.
    #[must_use]
    pub fn with_max_bytes(max_bytes: u64) -> Self {
        let mut directories = HashSet::new();
        directories.insert("/".to_string());
        Self {
            state: RwLock::new(StorageState {
                files: HashMap::new(),
                directories,
            }),
            max_bytes,
        }
    }

    /// The total-content cap, in bytes.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Capture the entire contents (files and directories) as a snapshot.
    ///
    /// The snapshot is an independent copy; later mutations to this storage do
    /// not affect it.
    pub async fn snapshot(&self) -> InMemorySnapshot {
        InMemorySnapshot {
            state: self.state.read().await.clone(),
        }
    }

    /// Replace this storage's entire contents with a snapshot, in place.
    ///
    /// The storage's identity is preserved, so any `Arc<dyn VfsStorage>` handles
    /// (e.g. those wired into a running sandbox) continue to refer to it and see
    /// the restored contents.
    pub async fn restore(&self, snapshot: &InMemorySnapshot) {
        *self.state.write().await = snapshot.state.clone();
    }

    /// Create a new, independent storage initialized from a snapshot.
    ///
    /// Useful for forking a session: the returned storage shares no state with
    /// the original, so the two can diverge freely.
    #[must_use]
    pub fn from_snapshot(snapshot: &InMemorySnapshot) -> Self {
        Self {
            state: RwLock::new(snapshot.state.clone()),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Normalize a path (remove trailing slashes, handle . and ..).
    pub(crate) fn normalize_path(path: &str) -> VfsResult<String> {
        if !path.starts_with('/') {
            return Err(VfsError::InvalidPath(format!(
                "path must be absolute: {path}"
            )));
        }

        let mut components: Vec<&str> = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => continue,
                ".." => {
                    if components.is_empty() {
                        return Err(VfsError::InvalidPath("path escapes root".to_string()));
                    }
                    components.pop();
                }
                c => components.push(c),
            }
        }

        if components.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{}", components.join("/")))
        }
    }

    /// Get the parent directory of a path.
    fn parent_path(path: &str) -> Option<String> {
        if path == "/" {
            return None;
        }
        let normalized = Self::normalize_path(path).ok()?;
        if normalized == "/" {
            return None;
        }
        match normalized.rfind('/') {
            Some(0) => Some("/".to_string()),
            Some(idx) => Some(normalized[..idx].to_string()),
            None => None,
        }
    }

    /// Check that resizing `path` to `new_len` keeps total content within
    /// `max_bytes`, returning `new_len` as a `usize` ready to resize with.
    ///
    /// Called before every allocation so that a guest-chosen offset or length
    /// can never turn into an unbounded host allocation. The current total is
    /// summed on the spot rather than tracked incrementally: it is only needed
    /// on the growth paths, and a running counter is one missed update away
    /// from being wrong.
    fn check_budget(&self, state: &StorageState, path: &str, new_len: u64) -> VfsResult<usize> {
        let current_len = state
            .files
            .get(path)
            .map_or(0, |f| f.content.len().try_into().unwrap_or(u64::MAX));

        // Only growth needs checking; shrinking always fits.
        if new_len > current_len {
            let others: u64 = state
                .files
                .iter()
                .filter(|(p, _)| p.as_str() != path)
                .map(|(_, f)| f.content.len().try_into().unwrap_or(u64::MAX))
                .fold(0u64, u64::saturating_add);

            let total = others.saturating_add(new_len);
            if total > self.max_bytes {
                return Err(VfsError::QuotaExceeded(format!(
                    "{path}: {new_len} bytes would put the filesystem at {total} bytes, over the {} byte limit",
                    self.max_bytes
                )));
            }
        }

        // new_len <= max_bytes, which is a u64 the host was willing to allocate,
        // so this only fails on a 32-bit host with an oversized cap.
        new_len.try_into().map_err(|_| {
            VfsError::QuotaExceeded(format!(
                "{path}: {new_len} bytes exceeds host addressable size"
            ))
        })
    }

    /// Hold the storage lock, so tests can exercise contention.
    #[cfg(test)]
    pub(crate) async fn hold_lock(&self) -> impl Drop + '_ {
        self.state.read().await
    }

    /// Check if parent directory exists (requires state to already be borrowed).
    fn check_parent_exists_with_state(state: &StorageState, path: &str) -> VfsResult<()> {
        if let Some(parent) = Self::parent_path(path)
            && !state.directories.contains(&parent)
        {
            return Err(VfsError::NotFound(format!("parent directory: {parent}")));
        }
        Ok(())
    }
}

#[async_trait]
impl VfsStorage for InMemoryStorage {
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    async fn read(&self, path: &str) -> VfsResult<Vec<u8>> {
        let path = Self::normalize_path(path)?;
        let state = self.state.read().await;
        match state.files.get(&path) {
            Some(data) => Ok(data.content.clone()),
            None => {
                if state.directories.contains(&path) {
                    Err(VfsError::NotFile(path))
                } else {
                    Err(VfsError::NotFound(path))
                }
            }
        }
    }

    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>> {
        let path = Self::normalize_path(path)?;
        let state = self.state.read().await;
        match state.files.get(&path) {
            Some(data) => {
                // Both offset and len come from the guest, so stay in u64 and
                // saturate: `offset + len` overflows for offsets near u64::MAX.
                let content_len = data.content.len().try_into().unwrap_or(u64::MAX);
                if offset >= content_len {
                    Ok(Vec::new())
                } else {
                    // offset < content_len, and end is clamped to it, so both
                    // fit in usize.
                    let end = offset.saturating_add(len).min(content_len);
                    Ok(data.content[offset as usize..end as usize].to_vec())
                }
            }
            None => {
                if state.directories.contains(&path) {
                    Err(VfsError::NotFile(path))
                } else {
                    Err(VfsError::NotFound(path))
                }
            }
        }
    }

    async fn write(&self, path: &str, data: &[u8]) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let mut state = self.state.write().await;

        Self::check_parent_exists_with_state(&state, &path)?;

        // Check it's not a directory
        if state.directories.contains(&path) {
            return Err(VfsError::NotFile(path));
        }

        self.check_budget(&state, &path, data.len().try_into().unwrap_or(u64::MAX))?;

        let now = SystemTime::now();
        let file_data = state.files.entry(path).or_insert_with(|| FileData {
            content: Vec::new(),
            created: now,
            modified: now,
            accessed: now,
        });
        file_data.content = data.to_vec();
        file_data.modified = now;
        Ok(())
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let mut state = self.state.write().await;

        Self::check_parent_exists_with_state(&state, &path)?;

        // Check it's not a directory
        if state.directories.contains(&path) {
            return Err(VfsError::NotFile(path));
        }

        // The guest picks the offset, so a one-byte write can ask the host to
        // materialize gigabytes of zeroes. Compute the end in u64 and check it
        // against the budget before allocating anything; an offset that
        // overflows u64 is over the limit by definition.
        let data_len: u64 = data.len().try_into().unwrap_or(u64::MAX);
        let end = offset.checked_add(data_len).ok_or_else(|| {
            VfsError::QuotaExceeded(format!(
                "{path}: write of {data_len} bytes at offset {offset} overflows the address space"
            ))
        })?;
        let needed_len = self.check_budget(&state, &path, end)?;

        let now = SystemTime::now();
        let file_data = state.files.entry(path).or_insert_with(|| FileData {
            content: Vec::new(),
            created: now,
            modified: now,
            accessed: now,
        });

        // Extend file if necessary
        if file_data.content.len() < needed_len {
            file_data.content.resize(needed_len, 0);
        }
        let start = needed_len - data.len();
        file_data.content[start..needed_len].copy_from_slice(data);
        file_data.modified = now;
        Ok(())
    }

    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let now = SystemTime::now();
        let mut state = self.state.write().await;
        if !state.files.contains_key(&path) {
            return Err(VfsError::NotFound(path));
        }

        // `size` is guest-controlled: truncating up to u64::MAX would either
        // abort the host on allocation failure or panic on capacity overflow.
        let size = self.check_budget(&state, &path, size)?;

        match state.files.get_mut(&path) {
            Some(data) => {
                data.content.resize(size, 0);
                data.modified = now;
                Ok(())
            }
            None => Err(VfsError::NotFound(path)),
        }
    }

    async fn delete(&self, path: &str) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let mut state = self.state.write().await;
        if state.files.remove(&path).is_some() {
            Ok(())
        } else if state.directories.contains(&path) {
            Err(VfsError::NotFile(path))
        } else {
            Err(VfsError::NotFound(path))
        }
    }

    async fn exists(&self, path: &str) -> VfsResult<bool> {
        let path = Self::normalize_path(path)?;
        let state = self.state.read().await;
        Ok(state.files.contains_key(&path) || state.directories.contains(&path))
    }

    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let path = Self::normalize_path(path)?;
        let state = self.state.read().await;

        // Check it's a directory
        if !state.directories.contains(&path) {
            if state.files.contains_key(&path) {
                return Err(VfsError::NotDirectory(path));
            } else {
                return Err(VfsError::NotFound(path));
            }
        }

        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };

        let mut entries = Vec::new();
        let mut seen_names = HashSet::new();

        // List files
        for (file_path, data) in &state.files {
            if let Some(rest) = file_path.strip_prefix(&prefix) {
                // Only include direct children (no more slashes)
                if !rest.contains('/') && !rest.is_empty() {
                    seen_names.insert(rest.to_string());
                    entries.push(DirEntry {
                        name: rest.to_string(),
                        metadata: Metadata {
                            is_dir: false,
                            size: data.content.len() as u64,
                            created: data.created,
                            modified: data.modified,
                            accessed: data.accessed,
                        },
                    });
                }
            }
        }

        // List subdirectories
        for dir_path in &state.directories {
            if let Some(rest) = dir_path.strip_prefix(&prefix) {
                // Only include direct children
                if !rest.contains('/') && !rest.is_empty() && !seen_names.contains(rest) {
                    let now = SystemTime::now();
                    entries.push(DirEntry {
                        name: rest.to_string(),
                        metadata: Metadata {
                            is_dir: true,
                            size: 0,
                            created: now,
                            modified: now,
                            accessed: now,
                        },
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let path = Self::normalize_path(path)?;
        let state = self.state.read().await;

        // Check files first
        if let Some(data) = state.files.get(&path) {
            return Ok(Metadata {
                is_dir: false,
                size: data.content.len() as u64,
                created: data.created,
                modified: data.modified,
                accessed: data.accessed,
            });
        }

        // Check directories
        if state.directories.contains(&path) {
            let now = SystemTime::now();
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                created: now,
                modified: now,
                accessed: now,
            });
        }

        Err(VfsError::NotFound(path))
    }

    async fn mkdir(&self, path: &str) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let mut state = self.state.write().await;

        Self::check_parent_exists_with_state(&state, &path)?;

        // Check if already exists
        if state.files.contains_key(&path) {
            return Err(VfsError::AlreadyExists(path));
        }
        if state.directories.contains(&path) {
            return Err(VfsError::AlreadyExists(path));
        }

        state.directories.insert(path);
        Ok(())
    }

    async fn rmdir(&self, path: &str) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;

        if path == "/" {
            return Err(VfsError::PermissionDenied(
                "cannot remove root directory".to_string(),
            ));
        }

        let mut state = self.state.write().await;

        // Check if it's a directory
        if !state.directories.contains(&path) {
            if state.files.contains_key(&path) {
                return Err(VfsError::NotDirectory(path));
            } else {
                return Err(VfsError::NotFound(path));
            }
        }

        // Check if empty
        let prefix = format!("{path}/");
        for file_path in state.files.keys() {
            if file_path.starts_with(&prefix) {
                return Err(VfsError::DirectoryNotEmpty(path));
            }
        }
        for dir_path in &state.directories {
            if dir_path.starts_with(&prefix) {
                return Err(VfsError::DirectoryNotEmpty(path));
            }
        }

        state.directories.remove(&path);
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from = Self::normalize_path(from)?;
        let to = Self::normalize_path(to)?;

        if from == to {
            return Ok(());
        }

        let mut state = self.state.write().await;

        Self::check_parent_exists_with_state(&state, &to)?;

        // Handle file rename
        if state.files.contains_key(&from) {
            // Check destination doesn't exist as directory
            if state.directories.contains(&to) {
                return Err(VfsError::AlreadyExists(to));
            }

            if let Some(data) = state.files.remove(&from) {
                state.files.insert(to, data);
                return Ok(());
            }
        }

        // Handle directory rename
        if state.directories.contains(&from) {
            if from == "/" {
                return Err(VfsError::PermissionDenied(
                    "cannot rename root directory".to_string(),
                ));
            }

            // Check destination doesn't exist as file
            if state.files.contains_key(&to) {
                return Err(VfsError::AlreadyExists(to));
            }

            // Rename directory and all contents
            let from_prefix = format!("{from}/");
            let to_prefix = format!("{to}/");

            // A directory can't be moved inside itself: the subtree would be
            // detached from the tree but keep its contents.
            if to.starts_with(&from_prefix) {
                return Err(VfsError::InvalidPath(format!(
                    "cannot move {from} into its own subdirectory {to}"
                )));
            }

            // An existing destination directory may only be replaced if empty.
            if state.directories.contains(&to)
                && (state.files.keys().any(|p| p.starts_with(&to_prefix))
                    || state.directories.iter().any(|p| p.starts_with(&to_prefix)))
            {
                return Err(VfsError::DirectoryNotEmpty(to));
            }

            // Rename files under the directory
            let files_to_rename: Vec<_> = state
                .files
                .keys()
                .filter(|p| p.starts_with(&from_prefix))
                .cloned()
                .collect();
            for old_path in files_to_rename {
                if let Some(data) = state.files.remove(&old_path) {
                    let new_path = old_path.replacen(&from_prefix, &to_prefix, 1);
                    state.files.insert(new_path, data);
                }
            }

            // Rename subdirectories
            let dirs_to_rename: Vec<_> = state
                .directories
                .iter()
                .filter(|p| *p == &from || p.starts_with(&from_prefix))
                .cloned()
                .collect();
            for old_path in dirs_to_rename {
                state.directories.remove(&old_path);
                let new_path = if old_path == from {
                    to.clone()
                } else {
                    old_path.replacen(&from_prefix, &to_prefix, 1)
                };
                state.directories.insert(new_path);
            }

            return Ok(());
        }

        Err(VfsError::NotFound(from))
    }

    async fn set_times(
        &self,
        path: &str,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let mut state = self.state.write().await;
        if let Some(file) = state.files.get_mut(&path) {
            if let Some(t) = accessed {
                file.accessed = t;
            }
            if let Some(t) = modified {
                file.modified = t;
            }
            Ok(())
        } else if state.directories.contains(&path) {
            // Directories don't store timestamps (stat reports none), so
            // there is nothing to update.
            Ok(())
        } else {
            Err(VfsError::NotFound(path))
        }
    }

    fn mkdir_sync(&self, path: &str) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;

        // Creates every missing ancestor, like `mkdir -p`.
        let create_all = |state: &mut StorageState| {
            let mut current = String::new();
            for component in path.split('/').filter(|s| !s.is_empty()) {
                current = format!("{current}/{component}");
                if state.files.contains_key(&current) {
                    return Err(VfsError::NotDirectory(current));
                }
                state.directories.insert(current.clone());
            }
            Ok(())
        };

        // Uncontended: take the lock without blocking.
        if let Ok(mut state) = self.state.try_write() {
            return create_all(&mut state);
        }

        match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            // Outside a runtime, blocking is fine.
            Err(_) => create_all(&mut self.state.blocking_write()),
            // `blocking_write` panics on a runtime thread unless it is inside
            // `block_in_place`, which also hands this worker's queued tasks
            // (one of which may hold the lock) to another worker.
            Ok(RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| create_all(&mut self.state.blocking_write()))
            }
            // Elsewhere the lock holder can only run on this thread, so
            // waiting for it would deadlock.
            Ok(_) => Err(VfsError::Busy(format!(
                "mkdir_sync {path}: storage is locked and waiting would block the \
                 only runtime thread; use the async mkdir instead"
            ))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Every offset here used to turn a one-byte write into a multi-gigabyte
    /// host allocation: 2^31 and 2^32 succeeded (2 GiB and 4 GiB of zeroes),
    /// 2^40 aborted the process outright ("memory allocation of 1099511627777
    /// bytes failed"), and u64::MAX panicked on `offset + len`. All of them are
    /// reachable from sandboxed Python via `f.seek(off); f.write(b'X')`.
    #[tokio::test]
    async fn write_at_extreme_offset_is_refused_not_allocated() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"small content").await.unwrap();

        for offset in [
            1u64 << 31,
            1 << 32,
            1 << 40,
            1 << 62,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let err = storage.write_at("/f", offset, b"X").await.unwrap_err();
            assert!(
                matches!(err, VfsError::QuotaExceeded(_)),
                "offset {offset} gave {err:?}"
            );
        }

        // The file is untouched by the refused writes.
        assert_eq!(storage.read("/f").await.unwrap(), b"small content");
        assert_eq!(storage.stat("/f").await.unwrap().size, 13);
    }

    #[tokio::test]
    async fn write_at_within_budget_still_extends_sparsely() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"abc").await.unwrap();

        storage.write_at("/f", 1000, b"X").await.unwrap();

        assert_eq!(storage.stat("/f").await.unwrap().size, 1001);
        let content = storage.read("/f").await.unwrap();
        assert_eq!(&content[..3], b"abc");
        assert!(content[3..1000].iter().all(|b| *b == 0));
        assert_eq!(content[1000], b'X');
    }

    #[tokio::test]
    async fn write_at_offset_overwrites_in_place() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"aaaaa").await.unwrap();

        storage.write_at("/f", 1, b"bb").await.unwrap();

        assert_eq!(storage.read("/f").await.unwrap(), b"abbaa");
        assert_eq!(storage.stat("/f").await.unwrap().size, 5);
    }

    #[tokio::test]
    async fn set_size_beyond_budget_is_refused() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"x").await.unwrap();

        // u64::MAX used to panic with "capacity overflow".
        for size in [u64::MAX, 1 << 62, DEFAULT_MAX_BYTES + 1] {
            let err = storage.set_size("/f", size).await.unwrap_err();
            assert!(
                matches!(err, VfsError::QuotaExceeded(_)),
                "size {size} gave {err:?}"
            );
        }

        assert_eq!(storage.stat("/f").await.unwrap().size, 1);
        // Shrinking is always allowed.
        storage.set_size("/f", 0).await.unwrap();
        assert_eq!(storage.stat("/f").await.unwrap().size, 0);
    }

    #[tokio::test]
    async fn rename_dir_into_own_subdirectory_is_refused() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/a").await.unwrap();
        storage.write("/a/f", b"x").await.unwrap();

        let err = storage.rename("/a", "/a/b").await.unwrap_err();
        assert!(matches!(err, VfsError::InvalidPath(_)), "{err:?}");
        assert_eq!(storage.read("/a/f").await.unwrap(), b"x");
    }

    #[tokio::test]
    async fn rename_dir_onto_dir_requires_empty_target() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/a").await.unwrap();
        storage.write("/a/f", b"from").await.unwrap();
        storage.mkdir("/b").await.unwrap();
        storage.write("/b/f", b"to").await.unwrap();

        let err = storage.rename("/a", "/b").await.unwrap_err();
        assert!(matches!(err, VfsError::DirectoryNotEmpty(_)), "{err:?}");
        assert_eq!(storage.read("/b/f").await.unwrap(), b"to");

        storage.delete("/b/f").await.unwrap();
        storage.rename("/a", "/b").await.unwrap();
        assert_eq!(storage.read("/b/f").await.unwrap(), b"from");
        assert!(!storage.exists("/a").await.unwrap());
    }

    #[tokio::test]
    async fn rename_root_is_refused() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/a").await.unwrap();
        let err = storage.rename("/", "/a").await.unwrap_err();
        assert!(matches!(err, VfsError::PermissionDenied(_)), "{err:?}");
    }

    #[tokio::test]
    async fn read_at_huge_length_does_not_overflow() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"small content").await.unwrap();

        // `offset + len` used to overflow and panic.
        assert_eq!(
            storage.read_at("/f", 5, u64::MAX).await.unwrap(),
            b" content"
        );
        assert_eq!(
            storage.read_at("/f", 0, u64::MAX).await.unwrap(),
            b"small content"
        );
        assert!(
            storage
                .read_at("/f", u64::MAX, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .read_at("/f", 13, u64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn budget_covers_all_files_together() {
        let storage = InMemoryStorage::with_max_bytes(1024);

        storage.write("/a", &vec![0u8; 600]).await.unwrap();
        // 600 + 600 > 1024, so the second file does not fit.
        let err = storage.write("/b", &vec![0u8; 600]).await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");

        // Freeing the first file makes room again.
        storage.delete("/a").await.unwrap();
        storage.write("/b", &vec![0u8; 600]).await.unwrap();

        // Overwriting a file counts its new size, not the sum of both.
        storage.write("/b", &vec![0u8; 1024]).await.unwrap();
        assert_eq!(storage.stat("/b").await.unwrap().size, 1024);
    }

    #[tokio::test]
    async fn max_bytes_is_reported() {
        assert_eq!(InMemoryStorage::new().max_bytes(), DEFAULT_MAX_BYTES);
        assert_eq!(InMemoryStorage::with_max_bytes(42).max_bytes(), 42);
    }

    #[tokio::test]
    async fn test_snapshot_restore_and_fork() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/data").await.unwrap();
        storage.write("/data/a.txt", b"original").await.unwrap();

        // Snapshot, then mutate.
        let snap = storage.snapshot().await;
        storage.write("/data/a.txt", b"changed").await.unwrap();
        storage.write("/data/b.txt", b"new").await.unwrap();

        // Restore in place: back to the snapshot, b.txt gone.
        storage.restore(&snap).await;
        assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"original");
        assert!(storage.read("/data/b.txt").await.is_err());

        // Fork: an independent copy that diverges without affecting the original.
        let forked = InMemoryStorage::from_snapshot(&snap);
        forked.write("/data/a.txt", b"forked").await.unwrap();
        assert_eq!(forked.read("/data/a.txt").await.unwrap(), b"forked");
        assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"original");
    }

    #[cfg(feature = "serde")]
    #[tokio::test]
    async fn test_snapshot_serde_round_trip() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/d").await.unwrap();
        storage.write("/d/f.txt", b"payload").await.unwrap();

        let snap = storage.snapshot().await;
        let bytes = serde_json::to_vec(&snap).unwrap();
        let restored: InMemorySnapshot = serde_json::from_slice(&bytes).unwrap();

        let fresh = InMemoryStorage::from_snapshot(&restored);
        assert_eq!(fresh.read("/d/f.txt").await.unwrap(), b"payload");
    }

    #[tokio::test]
    async fn test_file_operations() {
        let storage = InMemoryStorage::new();

        // Write and read
        storage.write("/test.txt", b"hello").await.unwrap();
        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(content, b"hello");

        // Read at offset
        let partial = storage.read_at("/test.txt", 2, 3).await.unwrap();
        assert_eq!(partial, b"llo");

        // Overwrite
        storage.write("/test.txt", b"world").await.unwrap();
        let content = storage.read("/test.txt").await.unwrap();
        assert_eq!(content, b"world");

        // Delete
        storage.delete("/test.txt").await.unwrap();
        assert!(storage.read("/test.txt").await.is_err());
    }

    #[tokio::test]
    async fn test_directory_operations() {
        let storage = InMemoryStorage::new();

        // Create directory
        storage.mkdir("/subdir").await.unwrap();
        assert!(storage.exists("/subdir").await.unwrap());

        // Create file in directory
        storage.write("/subdir/file.txt", b"content").await.unwrap();

        // List directory
        let entries = storage.list("/subdir").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");

        // Can't remove non-empty directory
        assert!(storage.rmdir("/subdir").await.is_err());

        // Remove file, then directory
        storage.delete("/subdir/file.txt").await.unwrap();
        storage.rmdir("/subdir").await.unwrap();
        assert!(!storage.exists("/subdir").await.unwrap());
    }

    #[tokio::test]
    async fn test_path_normalization() {
        let storage = InMemoryStorage::new();

        storage.write("/test.txt", b"data").await.unwrap();

        // Various path formats should work
        assert!(storage.exists("/test.txt").await.unwrap());
        assert!(storage.exists("/./test.txt").await.unwrap());

        // Parent references
        storage.mkdir("/dir").await.unwrap();
        storage.write("/dir/file.txt", b"data").await.unwrap();
        let content = storage.read("/dir/../dir/file.txt").await.unwrap();
        assert_eq!(content, b"data");
    }

    #[tokio::test]
    async fn test_rename() {
        let storage = InMemoryStorage::new();

        // File rename
        storage.write("/old.txt", b"content").await.unwrap();
        storage.rename("/old.txt", "/new.txt").await.unwrap();
        assert!(!storage.exists("/old.txt").await.unwrap());
        assert!(storage.exists("/new.txt").await.unwrap());

        // Directory rename
        storage.mkdir("/olddir").await.unwrap();
        storage.write("/olddir/file.txt", b"data").await.unwrap();
        storage.rename("/olddir", "/newdir").await.unwrap();
        assert!(!storage.exists("/olddir").await.unwrap());
        assert!(storage.exists("/newdir").await.unwrap());
        assert!(storage.exists("/newdir/file.txt").await.unwrap());
    }

    #[tokio::test]
    async fn test_stat() {
        let storage = InMemoryStorage::new();

        storage.write("/file.txt", b"hello").await.unwrap();
        let meta = storage.stat("/file.txt").await.unwrap();
        assert!(!meta.is_dir);
        assert_eq!(meta.size, 5);

        storage.mkdir("/dir").await.unwrap();
        let meta = storage.stat("/dir").await.unwrap();
        assert!(meta.is_dir);
    }

    #[tokio::test]
    async fn test_write_at() {
        let storage = InMemoryStorage::new();

        // Write at offset in new file
        storage.write_at("/file.txt", 5, b"world").await.unwrap();
        let content = storage.read("/file.txt").await.unwrap();
        assert_eq!(content.len(), 10);
        assert_eq!(&content[5..], b"world");
        assert_eq!(&content[0..5], &[0, 0, 0, 0, 0]);

        // Overwrite portion
        storage.write_at("/file.txt", 0, b"hello").await.unwrap();
        let content = storage.read("/file.txt").await.unwrap();
        assert_eq!(&content, b"helloworld");
    }

    #[test]
    fn test_mkdir_sync() {
        let storage = InMemoryStorage::new();

        // Create a directory synchronously
        storage.mkdir_sync("/data").unwrap();

        // Verify using blocking read
        let state = storage.state.blocking_read();
        assert!(state.directories.contains("/data"));
    }

    /// `blocking_write` and `Handle::block_on` both panic inside a runtime, so
    /// a contended lock used to take the host down.
    #[tokio::test(flavor = "multi_thread")]
    async fn mkdir_sync_waits_for_contended_lock_inside_runtime() {
        let storage = InMemoryStorage::new();
        let guard = storage.state.read().await;
        std::thread::scope(|s| {
            s.spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                drop(guard);
            });
            storage.mkdir_sync("/data/sub").unwrap();
        });
        assert!(storage.exists("/data/sub").await.unwrap());
    }

    /// On a current-thread runtime the lock holder can only run on the thread
    /// `mkdir_sync` would block, so waiting would hang forever.
    #[tokio::test(flavor = "current_thread")]
    async fn mkdir_sync_fails_instead_of_deadlocking_on_current_thread_runtime() {
        let storage = InMemoryStorage::new();
        let guard = storage.state.read().await;
        let err = storage.mkdir_sync("/data").unwrap_err();
        assert!(matches!(err, VfsError::Busy(_)), "{err:?}");
        drop(guard);

        storage.mkdir_sync("/data").unwrap();
        assert!(storage.exists("/data").await.unwrap());
    }

    #[tokio::test]
    async fn mkdir_sync_refuses_to_shadow_a_file() {
        let storage = InMemoryStorage::new();
        storage.write("/data", b"x").await.unwrap();
        let err = storage.mkdir_sync("/data/sub").unwrap_err();
        assert!(matches!(err, VfsError::NotDirectory(_)), "{err:?}");
        assert!(!storage.exists("/data/sub").await.unwrap());
    }

    #[test]
    fn test_mkdir_sync_nested() {
        let storage = InMemoryStorage::new();

        // Create nested directories synchronously
        storage.mkdir_sync("/data/subdir/nested").unwrap();

        // Verify all intermediate directories were created
        let state = storage.state.blocking_read();
        assert!(state.directories.contains("/data"));
        assert!(state.directories.contains("/data/subdir"));
        assert!(state.directories.contains("/data/subdir/nested"));
    }

    #[tokio::test]
    async fn set_times_updates_file_metadata() {
        let storage = InMemoryStorage::new();
        storage.write("/f", b"x").await.unwrap();
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);

        storage.set_times("/f", None, Some(t)).await.unwrap();
        assert_eq!(storage.stat("/f").await.unwrap().modified, t);
        assert!(matches!(
            storage.set_times("/missing", Some(t), None).await,
            Err(VfsError::NotFound(_))
        ));
    }
}
