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
///
/// Operations must be cancel-safe: the guest's non-blocking file streams poll
/// an operation once and, if it returns `Pending`, drop it and run it again
/// later from the start. A dropped operation must therefore leave no partial
/// effect that the rerun would duplicate (for example an append applied
/// twice), and must not keep holding any resource such as a lock.
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
    /// runtime: on a current-thread runtime, [`InMemoryStorage`] waits for a
    /// contended lock for a bounded time and then returns [`VfsError::Busy`].
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
    /// Bytes charged against the size limit, kept up to date by every change
    /// so checking the limit doesn't rescan the filesystem. Recomputed when a
    /// snapshot is applied.
    #[cfg_attr(feature = "serde", serde(skip))]
    used: u64,
}

impl StorageState {
    /// The bytes charged for the current contents, summed from scratch.
    fn usage(&self) -> u64 {
        let files = self
            .files
            .iter()
            .map(|(path, f)| file_cost(path, f.content.len()))
            .fold(0u64, u64::saturating_add);
        self.directories
            .iter()
            .filter(|path| path.as_str() != "/")
            .map(|path| entry_cost(path))
            .fold(files, u64::saturating_add)
    }

    fn entries(&self) -> usize {
        self.files.len() + self.directories.len()
    }
}

/// Bytes charged for a file or directory's existence, on top of its path and
/// any content.
///
/// Names and entries live in host memory just as file contents do, so they
/// count towards [`InMemoryStorage::max_bytes`]: otherwise empty files and
/// directories with long names would be free.
pub const ENTRY_OVERHEAD_BYTES: u64 = 64;

/// Most files and directories an [`InMemoryStorage`] holds, whatever its
/// size limit.
///
/// Some operations (listing, removing or renaming a directory) visit every
/// entry, so this keeps each of them cheap.
pub const MAX_ENTRIES: usize = 65_536;

/// Longest file or directory name accepted, in bytes (POSIX `NAME_MAX`).
const MAX_NAME_LEN: usize = 255;

/// Longest absolute path accepted, in bytes (POSIX `PATH_MAX`).
const MAX_PATH_LEN: usize = 4096;

/// Bytes charged for a directory, or for a file apart from its content.
fn entry_cost(path: &str) -> u64 {
    (path.len() as u64).saturating_add(ENTRY_OVERHEAD_BYTES)
}

/// Bytes charged for a file holding `len` bytes.
fn file_cost(path: &str, len: usize) -> u64 {
    entry_cost(path).saturating_add(len as u64)
}

/// Refuse a path too long to create (`ENAMETOOLONG`).
fn check_name_len(path: &str) -> VfsResult<()> {
    if path.len() > MAX_PATH_LEN || path.split('/').any(|c| c.len() > MAX_NAME_LEN) {
        return Err(VfsError::NameTooLong(format!(
            "{} byte path, limits are {MAX_NAME_LEN} per name and {MAX_PATH_LEN} in all",
            path.len()
        )));
    }
    Ok(())
}

/// How long [`InMemoryStorage::mkdir_sync`] waits for a contended lock on a
/// current-thread runtime before giving up.
const MKDIR_SYNC_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Default limit on the bytes an [`InMemoryStorage`] will hold: 64 MiB.
///
/// File contents live in host memory, and a sandboxed guest chooses both the
/// offsets it writes at and the sizes it truncates to. Without a limit, a
/// one-byte write at a large offset turns into an arbitrarily large host
/// allocation. Paths count too: each file and directory is charged its path
/// length plus [`ENTRY_OVERHEAD_BYTES`]. Override with
/// [`InMemoryStorage::with_max_bytes`].
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// In-memory VFS storage implementation.
///
/// Stores files and directories in memory using `HashMap` and `HashSet`.
/// Thread-safe via a single `RwLock` over the combined state.
///
/// Total size (file contents plus a charge per entry, see
/// [`ENTRY_OVERHEAD_BYTES`]) is capped at [`DEFAULT_MAX_BYTES`] unless changed
/// with [`InMemoryStorage::with_max_bytes`], and there are at most
/// [`MAX_ENTRIES`] files and directories. Operations that would exceed either
/// fail with [`VfsError::QuotaExceeded`], which guests see as `ENOSPC`. Names
/// longer than POSIX `NAME_MAX` (255 bytes) or paths longer than `PATH_MAX`
/// (4096 bytes) fail with [`VfsError::NameTooLong`].
#[derive(Debug)]
pub struct InMemoryStorage {
    /// Combined state under a single lock to prevent deadlocks.
    state: RwLock<StorageState>,
    /// Maximum total bytes charged, across all files and directories.
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
    /// The size limit of the storage the snapshot was taken from.
    #[cfg_attr(feature = "serde", serde(default = "default_max_bytes"))]
    max_bytes: u64,
}

#[cfg(feature = "serde")]
fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
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
    /// `max_bytes` bounds the total content of all files combined, plus the
    /// per-entry charge described on [`DEFAULT_MAX_BYTES`]. Because the
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
                used: 0,
            }),
            max_bytes,
        }
    }

    /// The size cap, in bytes.
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
            max_bytes: self.max_bytes,
        }
    }

    /// Replace this storage's entire contents with a snapshot, in place.
    ///
    /// The storage's identity is preserved, so any `Arc<dyn VfsStorage>` handles
    /// (e.g. those wired into a running sandbox) continue to refer to it and see
    /// the restored contents. This storage keeps its own size limit.
    ///
    /// # Errors
    ///
    /// [`VfsError::QuotaExceeded`] if the snapshot doesn't fit within this
    /// storage's limits; the storage is left unchanged.
    pub async fn restore(&self, snapshot: &InMemorySnapshot) -> VfsResult<()> {
        let state = Self::checked_state(snapshot, self.max_bytes)?;
        *self.state.write().await = state;
        Ok(())
    }

    /// Create a new, independent storage initialized from a snapshot, with
    /// the size limit of the storage the snapshot was taken from.
    ///
    /// Useful for forking a session: the returned storage shares no state with
    /// the original, so the two can diverge freely.
    ///
    /// # Errors
    ///
    /// [`VfsError::QuotaExceeded`] if the snapshot's contents exceed its own
    /// size limit or [`MAX_ENTRIES`], as a deserialized snapshot might.
    pub fn from_snapshot(snapshot: &InMemorySnapshot) -> VfsResult<Self> {
        Ok(Self {
            state: RwLock::new(Self::checked_state(snapshot, snapshot.max_bytes)?),
            max_bytes: snapshot.max_bytes,
        })
    }

    /// The snapshot's state with its usage recomputed, if it fits the limits.
    fn checked_state(snapshot: &InMemorySnapshot, max_bytes: u64) -> VfsResult<StorageState> {
        let mut state = snapshot.state.clone();
        state.used = state.usage();
        if state.used > max_bytes || state.entries() > MAX_ENTRIES {
            return Err(VfsError::QuotaExceeded(format!(
                "snapshot holds {} entries and {} bytes, over the limit of {MAX_ENTRIES} entries and {max_bytes} bytes",
                state.entries(),
                state.used
            )));
        }
        Ok(state)
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

    /// Check that resizing file `path` to `new_len` (creating it if needed)
    /// stays within the limits, returning `new_len` as a `usize` ready to
    /// resize with.
    ///
    /// Called before every allocation so that a guest-chosen offset or length
    /// can never turn into an unbounded host allocation.
    fn check_budget(&self, state: &StorageState, path: &str, new_len: u64) -> VfsResult<usize> {
        let current = match state.files.get(path) {
            Some(f) => file_cost(path, f.content.len()),
            None => {
                self.check_new_entry(state, path)?;
                0
            }
        };
        let new = entry_cost(path).saturating_add(new_len);
        self.check_growth(state, path, current, new)?;

        // new_len <= max_bytes, which is a u64 the host was willing to allocate,
        // so this only fails on a 32-bit host with an oversized cap.
        new_len.try_into().map_err(|_| {
            VfsError::QuotaExceeded(format!(
                "{path}: {new_len} bytes exceeds host addressable size"
            ))
        })
    }

    /// Check that replacing something charged `current` bytes with something
    /// charged `new` bytes stays within `max_bytes`. Shrinking always fits.
    fn check_growth(
        &self,
        state: &StorageState,
        path: &str,
        current: u64,
        new: u64,
    ) -> VfsResult<()> {
        if new > current {
            let total = state.used.saturating_sub(current).saturating_add(new);
            if total > self.max_bytes {
                return Err(VfsError::QuotaExceeded(format!(
                    "{path}: the filesystem would grow to {total} bytes, over the {} byte limit",
                    self.max_bytes
                )));
            }
        }
        Ok(())
    }

    /// Check that a new file or directory at `path` may be created.
    fn check_new_entry(&self, state: &StorageState, path: &str) -> VfsResult<()> {
        check_name_len(path)?;
        if state.entries() >= MAX_ENTRIES {
            return Err(VfsError::QuotaExceeded(format!(
                "{path}: the filesystem already holds {MAX_ENTRIES} entries"
            )));
        }
        Ok(())
    }

    /// Record that file `path` changed from `old_len` bytes (`None`: it
    /// didn't exist) to `new_len`.
    fn charge_file(state: &mut StorageState, path: &str, old_len: Option<usize>, new_len: usize) {
        let old = old_len.map_or(0, |len| file_cost(path, len));
        state.used = state
            .used
            .saturating_sub(old)
            .saturating_add(file_cost(path, new_len));
    }

    /// Hold the storage lock, so tests can exercise contention.
    #[cfg(test)]
    pub(crate) async fn hold_lock(&self) -> impl Drop + '_ {
        self.state.read().await
    }

    /// Hold the storage lock exclusively, so tests can contend with readers.
    #[cfg(test)]
    pub(crate) async fn hold_write_lock(&self) -> impl Drop + '_ {
        self.state.write().await
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

        let old_len = state.files.get(&path).map(|f| f.content.len());
        Self::charge_file(&mut state, &path, old_len, data.len());
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

        let old_len = state.files.get(&path).map(|f| f.content.len());
        let new_len = old_len.unwrap_or(0).max(needed_len);
        Self::charge_file(&mut state, &path, old_len, new_len);
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

        let old_len = state.files.get(&path).map(|f| f.content.len());
        Self::charge_file(&mut state, &path, old_len, size);
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
        if let Some(data) = state.files.remove(&path) {
            state.used = state
                .used
                .saturating_sub(file_cost(&path, data.content.len()));
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

        self.check_new_entry(&state, &path)?;
        self.check_growth(&state, &path, 0, entry_cost(&path))?;
        state.used = state.used.saturating_add(entry_cost(&path));
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
        state.used = state.used.saturating_sub(entry_cost(&path));
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

            check_name_len(&to)?;
            let len = state.files[&from].content.len();
            let replaced = state
                .files
                .get(&to)
                .map_or(0, |f| file_cost(&to, f.content.len()));
            let (current, new) = (file_cost(&from, len) + replaced, file_cost(&to, len));
            self.check_growth(&state, &to, current, new)?;

            if let Some(data) = state.files.remove(&from) {
                state.files.insert(to, data);
                state.used = state.used.saturating_sub(current).saturating_add(new);
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

            let files_to_rename: Vec<_> = state
                .files
                .keys()
                .filter(|p| p.starts_with(&from_prefix))
                .cloned()
                .collect();

            let dirs_to_rename: Vec<_> = state
                .directories
                .iter()
                .filter(|p| *p == &from || p.starts_with(&from_prefix))
                .cloned()
                .collect();

            // Every moved path changes length by the same amount; an existing
            // (empty) destination directory is replaced.
            check_name_len(&to)?;
            let longest = files_to_rename
                .iter()
                .chain(&dirs_to_rename)
                .map(String::len)
                .max()
                .unwrap_or(0);
            if longest - from.len() + to.len() > MAX_PATH_LEN {
                return Err(VfsError::NameTooLong(format!(
                    "moving {from} to {to} makes paths longer than {MAX_PATH_LEN} bytes"
                )));
            }
            let moved = (files_to_rename.len() + dirs_to_rename.len()) as u64;
            let replaced = if state.directories.contains(&to) {
                entry_cost(&to)
            } else {
                0
            };
            let current = (from.len() as u64)
                .saturating_mul(moved)
                .saturating_add(replaced);
            let new = (to.len() as u64).saturating_mul(moved);
            self.check_growth(&state, &to, current, new)?;
            state.used = state.used.saturating_sub(current).saturating_add(new);

            for old_path in files_to_rename {
                if let Some(data) = state.files.remove(&old_path) {
                    let new_path = old_path.replacen(&from_prefix, &to_prefix, 1);
                    state.files.insert(new_path, data);
                }
            }

            // Rename subdirectories
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
                if !state.directories.contains(&current) {
                    self.check_new_entry(state, &current)?;
                    self.check_growth(state, &current, 0, entry_cost(&current))?;
                    state.used = state.used.saturating_add(entry_cost(&current));
                    state.directories.insert(current.clone());
                }
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
            // Elsewhere a holder on this runtime can't run while this thread
            // blocks, so only poll for a while: a lock held briefly by another
            // thread still succeeds, and a same-runtime holder gets an error
            // instead of a deadlock.
            Ok(_) => {
                let deadline = std::time::Instant::now() + MKDIR_SYNC_WAIT;
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    if let Ok(mut state) = self.state.try_write() {
                        return create_all(&mut state);
                    }
                }
                Err(VfsError::Busy(format!(
                    "mkdir_sync {path}: storage stayed locked for {MKDIR_SYNC_WAIT:?}; \
                     use the async mkdir from inside a current-thread runtime"
                )))
            }
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
        storage.write("/b", &vec![0u8; 900]).await.unwrap();
        assert_eq!(storage.stat("/b").await.unwrap().size, 900);
    }

    /// Empty files and directories, and their names, are charged too, so
    /// they can't grow host memory past the limit for free.
    #[tokio::test]
    async fn entries_and_names_count_towards_the_limit() {
        let storage = InMemoryStorage::with_max_bytes(10 * (ENTRY_OVERHEAD_BYTES + 3));
        for i in 0..10 {
            storage.mkdir(&format!("/d{i}")).await.unwrap();
        }
        let err = storage.write("/d", b"").await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");
        let err = storage.mkdir("/dx").await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");
        // A longer name needs more room.
        let err = storage.rename("/d1", "/d10").await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");
        storage.rmdir("/d0").await.unwrap();
        storage.write("/f", b"").await.unwrap();
    }

    #[tokio::test]
    async fn overlong_names_are_refused() {
        let storage = InMemoryStorage::new();
        let name = "n".repeat(MAX_NAME_LEN + 1);
        for result in [
            storage.write(&format!("/{name}"), b"").await,
            storage.mkdir(&format!("/{name}")).await,
            storage.mkdir_sync(&format!("/ok/{name}")),
        ] {
            assert!(
                matches!(result, Err(VfsError::NameTooLong(_))),
                "{result:?}"
            );
        }

        // Each name fits, but the path is too long.
        let mut path = String::new();
        let part = "p".repeat(MAX_NAME_LEN);
        while path.len() + part.len() < MAX_PATH_LEN {
            path = format!("{path}/{part}");
            storage.mkdir(&path).await.unwrap();
        }
        let err = storage.mkdir(&format!("{path}/{part}")).await.unwrap_err();
        assert!(matches!(err, VfsError::NameTooLong(_)), "{err:?}");
        // Moving the deep tree under a longer name would overflow it too.
        storage.mkdir("/long-parent-name").await.unwrap();
        let err = storage
            .rename(&format!("/{part}"), &format!("/long-parent-name/{part}"))
            .await
            .unwrap_err();
        assert!(matches!(err, VfsError::NameTooLong(_)), "{err:?}");
    }

    #[tokio::test]
    async fn entry_count_is_capped() {
        let storage = InMemoryStorage::with_max_bytes(u64::MAX);
        // The root directory is the first entry.
        for i in 1..MAX_ENTRIES {
            storage.write(&format!("/{i}"), b"").await.unwrap();
        }
        let err = storage.mkdir("/one-more").await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");
        storage.delete("/1").await.unwrap();
        storage.mkdir("/one-more").await.unwrap();
    }

    /// The running total must match a recount after every kind of change.
    #[tokio::test]
    async fn running_usage_matches_recount() {
        let storage = InMemoryStorage::new();
        let check = || async {
            let state = storage.state.read().await;
            assert_eq!(state.used, state.usage());
        };
        storage.mkdir("/a").await.unwrap();
        storage.write("/a/f", b"hello").await.unwrap();
        storage.write("/a/f", b"hi").await.unwrap();
        storage.write_at("/a/g", 10, b"x").await.unwrap();
        storage.write_at("/a/g", 0, b"y").await.unwrap();
        storage.set_size("/a/f", 100).await.unwrap();
        check().await;
        storage.rename("/a/f", "/a/longer-name").await.unwrap();
        storage.rename("/a/g", "/a/longer-name").await.unwrap();
        storage.mkdir("/b").await.unwrap();
        storage.rename("/a", "/b").await.unwrap();
        storage.rename("/b", "/c-is-longer").await.unwrap();
        check().await;
        storage.mkdir_sync("/x/y/z").unwrap();
        storage.mkdir_sync("/x/y/z/w").unwrap();
        storage.delete("/c-is-longer/longer-name").await.unwrap();
        storage.rmdir("/c-is-longer").await.unwrap();
        check().await;
        assert_eq!(
            storage.state.read().await.used,
            ["/x", "/x/y", "/x/y/z", "/x/y/z/w"]
                .iter()
                .map(|p| entry_cost(p))
                .sum::<u64>()
        );
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
        storage.restore(&snap).await.unwrap();
        assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"original");
        assert!(storage.read("/data/b.txt").await.is_err());

        // Fork: an independent copy that diverges without affecting the original.
        let forked = InMemoryStorage::from_snapshot(&snap).unwrap();
        forked.write("/data/a.txt", b"forked").await.unwrap();
        assert_eq!(forked.read("/data/a.txt").await.unwrap(), b"forked");
        assert_eq!(storage.read("/data/a.txt").await.unwrap(), b"original");
    }

    #[tokio::test]
    async fn snapshots_keep_and_respect_the_size_limit() {
        let storage = InMemoryStorage::with_max_bytes(4096);
        storage.write("/f", &[0u8; 2048]).await.unwrap();
        let snap = storage.snapshot().await;

        // A fork keeps the original's limit.
        let forked = InMemoryStorage::from_snapshot(&snap).unwrap();
        assert_eq!(forked.max_bytes(), 4096);
        let err = forked.write("/g", &[0u8; 2048]).await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");

        // A storage with a smaller limit refuses the snapshot and stays as it was.
        let small = InMemoryStorage::with_max_bytes(1024);
        small.write("/kept", b"x").await.unwrap();
        let err = small.restore(&snap).await.unwrap_err();
        assert!(matches!(err, VfsError::QuotaExceeded(_)), "{err:?}");
        assert_eq!(small.read("/kept").await.unwrap(), b"x");
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

        let fresh = InMemoryStorage::from_snapshot(&restored).unwrap();
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

    /// On a current-thread runtime a holder on the same runtime can't run
    /// while `mkdir_sync` blocks, so waiting for it would hang forever.
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

    /// A lock held briefly by another thread is still waited for on a
    /// current-thread runtime.
    #[tokio::test(flavor = "current_thread")]
    async fn mkdir_sync_waits_for_other_thread_on_current_thread_runtime() {
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
