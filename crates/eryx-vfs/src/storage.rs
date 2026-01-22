//! VFS storage trait and implementations.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use async_trait::async_trait;
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

    /// Synchronously create a directory.
    ///
    /// This is useful for setup code that runs before the async runtime
    /// is available. The default implementation returns an error - implementors
    /// should override this if they support sync directory creation.
    fn mkdir_sync(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::Storage(
            "mkdir_sync not implemented for this storage backend".to_string(),
        ))
    }
}

/// Internal state for in-memory storage.
///
/// Combining files and directories into a single struct allows us to use
/// a single `RwLock`, avoiding potential deadlock issues from acquiring
/// multiple locks.
#[derive(Debug, Default)]
struct StorageState {
    /// File contents: path -> data
    files: HashMap<String, FileData>,
    /// Directory markers: set of directory paths
    directories: HashSet<String>,
}

/// In-memory VFS storage implementation.
///
/// Stores files and directories in memory using `HashMap` and `HashSet`.
/// Thread-safe via a single `RwLock` over the combined state.
#[derive(Debug)]
pub struct InMemoryStorage {
    /// Combined state under a single lock to prevent deadlocks.
    state: RwLock<StorageState>,
}

#[derive(Debug, Clone)]
struct FileData {
    content: Vec<u8>,
    created: SystemTime,
    modified: SystemTime,
    accessed: SystemTime,
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage with root directory.
    #[must_use]
    pub fn new() -> Self {
        let mut directories = HashSet::new();
        directories.insert("/".to_string());
        Self {
            state: RwLock::new(StorageState {
                files: HashMap::new(),
                directories,
            }),
        }
    }

    /// Normalize a path (remove trailing slashes, handle . and ..).
    fn normalize_path(path: &str) -> VfsResult<String> {
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
                let offset = offset as usize;
                let len = len as usize;
                if offset >= data.content.len() {
                    Ok(Vec::new())
                } else {
                    let end = (offset + len).min(data.content.len());
                    Ok(data.content[offset..end].to_vec())
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

        let now = SystemTime::now();
        let offset = offset as usize;
        let file_data = state.files.entry(path).or_insert_with(|| FileData {
            content: Vec::new(),
            created: now,
            modified: now,
            accessed: now,
        });

        // Extend file if necessary
        let needed_len = offset + data.len();
        if file_data.content.len() < needed_len {
            file_data.content.resize(needed_len, 0);
        }
        file_data.content[offset..offset + data.len()].copy_from_slice(data);
        file_data.modified = now;
        Ok(())
    }

    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;
        let now = SystemTime::now();
        let mut state = self.state.write().await;
        match state.files.get_mut(&path) {
            Some(data) => {
                data.content.resize(size as usize, 0);
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
            // Check destination doesn't exist as file
            if state.files.contains_key(&to) {
                return Err(VfsError::AlreadyExists(to));
            }

            // Rename directory and all contents
            let from_prefix = format!("{from}/");
            let to_prefix = format!("{to}/");

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

    fn mkdir_sync(&self, path: &str) -> VfsResult<()> {
        let path = Self::normalize_path(path)?;

        // Collect all directories to create first.
        let mut dirs_to_create = Vec::new();
        let mut current = String::new();
        for component in path.split('/').filter(|s| !s.is_empty()) {
            current = format!("{}/{}", current, component);
            dirs_to_create.push(current.clone());
        }

        // Try to acquire the lock without blocking first.
        // This handles the common case where no contention exists.
        if let Ok(mut state) = self.state.try_write() {
            for dir in dirs_to_create {
                state.directories.insert(dir);
            }
            return Ok(());
        }

        // If try_write fails, we need to use a blocking approach.
        // Check if we're in an async runtime context.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // We're in an async runtime - use block_on with async write.
            // This is safe because mkdir_sync is only called during setup,
            // before the store starts processing async WASM calls.
            handle.block_on(async {
                let mut state = self.state.write().await;
                for dir in dirs_to_create {
                    state.directories.insert(dir);
                }
            });
        } else {
            // Not in an async runtime - safe to use blocking_write.
            let mut state = self.state.blocking_write();
            for dir in dirs_to_create {
                state.directories.insert(dir);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
}
