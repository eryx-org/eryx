//! A copy-on-write overlay of a read-only storage.
//!
//! [`OverlayStorage`] serves a directory of a lower storage, which it never
//! writes to, with the writes going to an [`InMemoryStorage`] upper layer and
//! deletions recorded as whiteouts over the lower one. Reads see the upper
//! layer first, then the lower one minus whiteouts. [`OverlayStorage::changes`]
//! lists what was done, for a caller that wants to apply it elsewhere.
//!
//! The rules, overlayfs-like but simpler:
//! - Writing part of a lower file first copies it up whole; replacing it,
//!   or truncating it to nothing, doesn't. So does setting its times.
//! - A deleted lower file or directory becomes a whiteout. `rmdir` needs the
//!   merged directory to be empty, so a removed directory's lower entries
//!   are all whited out already, and recreating it doesn't bring them back.
//! - Renaming a file copies it up and whites out the old name. Renaming a
//!   directory made in the overlay is a plain rename; renaming a lower
//!   directory is `EXDEV` (code that moves trees copies instead).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::error::{VfsError, VfsResult};
use crate::storage::{DirEntry, InMemoryStorage, Metadata, VfsStorage};

/// A copy-on-write overlay: a read-only lower storage under an in-memory
/// upper layer that takes the writes.
///
/// The overlay serves `root` and what's beneath it, at the same paths as in
/// the lower storage. The lower storage is only read, so one can sit under
/// many overlays at once.
///
/// # As a sandbox volume
///
/// With the `eryx` crate, mount one as a storage volume
/// (`VolumeMount::storage`). To give many sandboxes the same starting files
/// but each its own writes, share one lower storage and make a fresh upper
/// layer for each run:
///
/// ```rust
/// # async fn example() -> eryx_vfs::VfsResult<()> {
/// use std::sync::Arc;
/// use eryx_vfs::{InMemoryStorage, OverlayStorage, VfsStorage};
///
/// // Made once, shared by every run
/// let cache = InMemoryStorage::new();
/// cache.mkdir_sync("/cache")?;
/// cache.write("/cache/prebuilt", b"...").await?;
/// let cache: Arc<dyn VfsStorage> = Arc::new(cache);
///
/// // Per run: its writes, at most 64 MiB of them, go to its own upper layer
/// let upper = Arc::new(InMemoryStorage::with_max_bytes(64 << 20));
/// let overlay = OverlayStorage::new("/cache", Arc::clone(&cache), upper)?;
/// overlay.write("/cache/new", b"entry").await?;
/// assert!(!cache.exists("/cache/new").await?);
/// // eryx::VolumeMount::storage("/cache", Arc::new(overlay))
/// # Ok(())
/// # }
/// ```
///
/// Copy-ups are whole files, held in memory. Setting a lower file's times
/// copies it up too, so code that touches every file it reads copies them
/// all.
pub struct OverlayStorage {
    /// The directory the overlay serves
    root: String,
    lower: Arc<dyn VfsStorage>,
    /// The writes; may be shared by several overlays (with different roots),
    /// whose limit is then for them all
    upper: Arc<InMemoryStorage>,
    state: Mutex<State>,
    /// Held while copying a lower file up, so two writers to it can't both
    /// copy it, the second over the first's write
    copying: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for OverlayStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayStorage")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// One thing an [`OverlayStorage`] changed, by its path in the storage.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverlayChange {
    /// The lower file or directory there was deleted.
    Remove(String),
    /// A directory was made there.
    Mkdir(String),
    /// The file there was written; its contents are the overlay's (read
    /// them with [`VfsStorage::read`]).
    Write(String),
}

/// What was done besides writing files, by path
#[derive(Default)]
struct State {
    /// Lower entries deleted
    whiteouts: BTreeSet<String>,
    /// Directories made (the upper layer also has its parents, to hold its
    /// files, which aren't changes)
    created: BTreeSet<String>,
}

/// A poisoned lock only means another operation panicked; the sets are
/// still consistent
fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where `path` lives, in the merged view
enum Found {
    Upper(Metadata),
    Lower(Metadata),
    Nowhere,
}

impl Found {
    fn metadata(&self) -> Option<&Metadata> {
        match self {
            Found::Upper(m) | Found::Lower(m) => Some(m),
            Found::Nowhere => None,
        }
    }
}

impl OverlayStorage {
    /// An overlay of `lower`'s directory `root`, keeping its changes in
    /// `upper`. A copy-up bigger than `upper`'s
    /// [`max_bytes`](InMemoryStorage::max_bytes) fails before reading the
    /// file.
    ///
    /// # Errors
    ///
    /// If the upper layer can't hold `root`.
    pub fn new(
        root: &str,
        lower: Arc<dyn VfsStorage>,
        upper: Arc<InMemoryStorage>,
    ) -> VfsResult<Self> {
        upper.mkdir_sync(root)?;
        Ok(Self {
            root: root.to_string(),
            lower,
            upper,
            state: Mutex::default(),
            copying: tokio::sync::Mutex::new(()),
        })
    }

    /// The directory the overlay serves.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    async fn find(&self, path: &str) -> VfsResult<Found> {
        match self.upper.stat(path).await {
            Ok(m) => return Ok(Found::Upper(m)),
            Err(VfsError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        if lock(&self.state).whiteouts.contains(path) {
            return Ok(Found::Nowhere);
        }
        match self.lower.stat(path).await {
            Ok(m) => Ok(Found::Lower(m)),
            Err(VfsError::NotFound(_) | VfsError::NotDirectory(_)) => Ok(Found::Nowhere),
            Err(e) => Err(e),
        }
    }

    /// Make `path`'s parent directories in the upper layer, each of which
    /// must already be a directory in the merged view
    async fn ensure_parent(&self, path: &str) -> VfsResult<()> {
        let Some((parent, _)) = path.rsplit_once('/') else {
            return Err(VfsError::InvalidPath(path.to_string()));
        };
        if parent.len() <= self.root.len() {
            return Ok(());
        }
        let mut dir = self.root.clone();
        for component in parent[self.root.len()..]
            .split('/')
            .filter(|c| !c.is_empty())
        {
            dir.push('/');
            dir.push_str(component);
            match self.find(&dir).await? {
                Found::Upper(m) if m.is_dir => {}
                Found::Lower(m) if m.is_dir => match self.upper.mkdir(&dir).await {
                    Ok(()) | Err(VfsError::AlreadyExists(_)) => {}
                    Err(e) => return Err(e),
                },
                Found::Upper(_) | Found::Lower(_) => return Err(VfsError::NotDirectory(dir)),
                Found::Nowhere => return Err(VfsError::NotFound(dir)),
            }
        }
        Ok(())
    }

    /// Have `path` in the upper layer, copying a lower file up whole
    async fn copy_up(&self, path: &str) -> VfsResult<()> {
        if let Found::Upper(m) = self.find(path).await?
            && !m.is_dir
        {
            return Ok(());
        }
        // Looked up again under the lock: another writer may have copied it
        let _copying = self.copying.lock().await;
        match self.find(path).await? {
            Found::Upper(m) | Found::Lower(m) if m.is_dir => {
                Err(VfsError::NotFile(path.to_string()))
            }
            Found::Upper(_) => Ok(()),
            Found::Lower(m) => {
                if m.size > self.upper.max_bytes() {
                    return Err(VfsError::QuotaExceeded(format!(
                        "{path}: copying it to write it needs {} bytes",
                        m.size
                    )));
                }
                let data = self.lower.read(path).await?;
                self.ensure_parent(path).await?;
                self.upper.write(path, &data).await
            }
            Found::Nowhere => Err(VfsError::NotFound(path.to_string())),
        }
    }

    async fn is_dir(&self, path: &str) -> VfsResult<bool> {
        Ok(self.find(path).await?.metadata().is_some_and(|m| m.is_dir))
    }

    /// Whether the lower storage has something at `path` (deleted or not)
    async fn lower_has(&self, path: &str) -> VfsResult<bool> {
        match self.lower.stat(path).await {
            Ok(_) => Ok(true),
            Err(VfsError::NotFound(_) | VfsError::NotDirectory(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The merged listing of the directory `path`
    async fn merged(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let found = self.find(path).await?;
        match found.metadata() {
            None => return Err(VfsError::NotFound(path.to_string())),
            Some(m) if !m.is_dir => return Err(VfsError::NotDirectory(path.to_string())),
            Some(_) => {}
        }
        let mut entries = std::collections::BTreeMap::new();
        // A directory made (or remade) in the overlay has no lower entries
        // left to show; any other is the lower one's, even with an upper
        // copy holding new files, so its listing must come
        let made = lock(&self.state).created.contains(path);
        let lower = match self.lower.list(path).await {
            Ok(lower) => lower,
            Err(VfsError::NotFound(_) | VfsError::NotDirectory(_)) => Vec::new(),
            Err(_) if made => Vec::new(),
            Err(e) => return Err(e),
        };
        {
            let state = lock(&self.state);
            for entry in lower {
                let child = format!("{}/{}", path.trim_end_matches('/'), entry.name);
                if !state.whiteouts.contains(&child) {
                    entries.insert(entry.name.clone(), entry);
                }
            }
        }
        if matches!(found, Found::Upper(_)) {
            for entry in self.upper.list(path).await? {
                entries.insert(entry.name.clone(), entry);
            }
        }
        Ok(entries.into_values().collect())
    }

    /// What the overlay changed, in an order that can be applied as it
    /// goes: removals deepest first, then new directories parents first,
    /// then the written files, by path.
    ///
    /// # Errors
    ///
    /// If the upper layer can't be listed.
    pub async fn changes(&self) -> VfsResult<Vec<OverlayChange>> {
        let (whiteouts, created) = {
            let state = lock(&self.state);
            (state.whiteouts.clone(), state.created.clone())
        };
        let mut removed: Vec<String> = whiteouts.into_iter().collect();
        removed.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
        let mut changes: Vec<OverlayChange> =
            removed.into_iter().map(OverlayChange::Remove).collect();
        for dir in created {
            if matches!(self.upper.stat(&dir).await, Ok(m) if m.is_dir) {
                changes.push(OverlayChange::Mkdir(dir));
            }
        }
        // Every file in the upper layer was written (or copied up to be)
        let mut dirs = vec![self.root.clone()];
        let mut files = Vec::new();
        while let Some(dir) = dirs.pop() {
            for entry in self.upper.list(&dir).await? {
                let path = format!("{dir}/{}", entry.name);
                if entry.metadata.is_dir {
                    dirs.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        changes.extend(files.into_iter().map(OverlayChange::Write));
        Ok(changes)
    }
}

#[async_trait]
impl VfsStorage for OverlayStorage {
    async fn read(&self, path: &str) -> VfsResult<Vec<u8>> {
        match self.find(path).await? {
            Found::Upper(_) => self.upper.read(path).await,
            Found::Lower(_) => self.lower.read(path).await,
            Found::Nowhere => Err(VfsError::NotFound(path.to_string())),
        }
    }

    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>> {
        match self.find(path).await? {
            Found::Upper(_) => self.upper.read_at(path, offset, len).await,
            Found::Lower(_) => self.lower.read_at(path, offset, len).await,
            Found::Nowhere => Err(VfsError::NotFound(path.to_string())),
        }
    }

    async fn write(&self, path: &str, data: &[u8]) -> VfsResult<()> {
        if self.is_dir(path).await? {
            return Err(VfsError::NotFile(path.to_string()));
        }
        self.ensure_parent(path).await?;
        self.upper.write(path, data).await
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        match self.copy_up(path).await {
            Ok(()) => {}
            // A new file
            Err(VfsError::NotFound(_)) => self.ensure_parent(path).await?,
            Err(e) => return Err(e),
        }
        self.upper.write_at(path, offset, data).await
    }

    async fn append(&self, path: &str, data: &[u8]) -> VfsResult<u64> {
        match self.copy_up(path).await {
            Ok(()) => {}
            Err(VfsError::NotFound(_)) => self.ensure_parent(path).await?,
            Err(e) => return Err(e),
        }
        self.upper.append(path, data).await
    }

    async fn create_file(&self, path: &str, exclusive: bool) -> VfsResult<bool> {
        if let Found::Lower(_) = self.find(path).await? {
            return if exclusive {
                Err(VfsError::AlreadyExists(path.to_string()))
            } else {
                Ok(false)
            };
        }
        self.ensure_parent(path).await?;
        self.upper.create_file(path, exclusive).await
    }

    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()> {
        // Truncating a lower file to nothing (`open(..., 'w')`) needn't
        // read it first
        if size == 0
            && let Found::Lower(m) = self.find(path).await?
            && !m.is_dir
        {
            self.ensure_parent(path).await?;
            return self.upper.write(path, &[]).await;
        }
        self.copy_up(path).await?;
        self.upper.set_size(path, size).await
    }

    async fn delete(&self, path: &str) -> VfsResult<()> {
        let found = self.find(path).await?;
        if found.metadata().is_some_and(|m| m.is_dir) {
            return Err(VfsError::NotFile(path.to_string()));
        }
        if let Found::Upper(_) = found {
            self.upper.delete(path).await?;
        }
        if let Found::Nowhere = found {
            return Err(VfsError::NotFound(path.to_string()));
        }
        // Whatever the lower storage has there goes too
        if self.lower_has(path).await? {
            lock(&self.state).whiteouts.insert(path.to_string());
        }
        Ok(())
    }

    async fn exists(&self, path: &str) -> VfsResult<bool> {
        Ok(self.find(path).await?.metadata().is_some())
    }

    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.merged(path).await
    }

    async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.find(path)
            .await?
            .metadata()
            .cloned()
            .ok_or_else(|| VfsError::NotFound(path.to_string()))
    }

    async fn mkdir(&self, path: &str) -> VfsResult<()> {
        if self.find(path).await?.metadata().is_some() {
            return Err(VfsError::AlreadyExists(path.to_string()));
        }
        self.ensure_parent(path).await?;
        self.upper.mkdir(path).await?;
        lock(&self.state).created.insert(path.to_string());
        Ok(())
    }

    async fn rmdir(&self, path: &str) -> VfsResult<()> {
        if !self.merged(path).await?.is_empty() {
            return Err(VfsError::DirectoryNotEmpty(path.to_string()));
        }
        if let Found::Upper(_) = self.find(path).await? {
            self.upper.rmdir(path).await?;
        }
        let lower = self.lower_has(path).await?;
        let mut state = lock(&self.state);
        state.created.remove(path);
        if lower {
            state.whiteouts.insert(path.to_string());
        }
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let found = self.find(from).await?;
        let Some(meta) = found.metadata().cloned() else {
            return Err(VfsError::NotFound(from.to_string()));
        };
        if meta.is_dir {
            // Only a directory wholly the overlay's moves as one
            let made = lock(&self.state).created.contains(from);
            if !made || self.lower_has(from).await? {
                return Err(VfsError::CrossDevice(format!(
                    "{from}: a lower directory can't be renamed, only copied"
                )));
            }
            // As rename(2): onto nothing, or onto an empty directory, which
            // goes
            match self.find(to).await? {
                Found::Nowhere => {}
                Found::Upper(m) | Found::Lower(m) if !m.is_dir => {
                    return Err(VfsError::NotDirectory(to.to_string()));
                }
                _ => self.rmdir(to).await?,
            }
            self.ensure_parent(to).await?;
            self.upper.rename(from, to).await?;
            let mut state = lock(&self.state);
            let moved: Vec<String> = state
                .created
                .iter()
                .filter(|p| *p == from || p.starts_with(&format!("{from}/")))
                .cloned()
                .collect();
            for p in moved {
                state.created.remove(&p);
                state.created.insert(format!("{to}{}", &p[from.len()..]));
            }
            return Ok(());
        }
        if self.is_dir(to).await? {
            return Err(VfsError::NotFile(to.to_string()));
        }
        // The destination first, so a rename that fails copies nothing up
        self.ensure_parent(to).await?;
        self.copy_up(from).await?;
        self.upper.rename(from, to).await?;
        if self.lower_has(from).await? {
            lock(&self.state).whiteouts.insert(from.to_string());
        }
        Ok(())
    }

    async fn set_times(
        &self,
        path: &str,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> VfsResult<()> {
        // A lower directory's times are left alone: they aren't a change
        // `changes` could report
        if self.is_dir(path).await? {
            return match self.upper.set_times(path, accessed, modified).await {
                Ok(()) | Err(VfsError::NotFound(_)) => Ok(()),
                Err(e) => Err(e),
            };
        }
        self.copy_up(path).await?;
        self.upper.set_times(path, accessed, modified).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn new(lower: InMemoryStorage, max_bytes: u64) -> OverlayStorage {
        let upper = Arc::new(InMemoryStorage::with_max_bytes(max_bytes));
        OverlayStorage::new("/mnt/m", Arc::new(lower), upper).unwrap()
    }

    /// An overlay at `/mnt/m` over a lower tree with `a.txt`, `dir/b.txt`
    /// and the empty `empty/`
    async fn overlay() -> OverlayStorage {
        let lower = InMemoryStorage::new();
        lower.mkdir_sync("/mnt/m/dir").unwrap();
        lower.mkdir_sync("/mnt/m/empty").unwrap();
        lower.write("/mnt/m/a.txt", b"alpha").await.unwrap();
        lower.write("/mnt/m/dir/b.txt", b"beta").await.unwrap();
        new(lower, 1024 * 1024)
    }

    async fn names(o: &OverlayStorage, path: &str) -> Vec<String> {
        o.list(path)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    /// The changes as `(path relative to the root, kind, contents)`
    async fn changes(o: &OverlayStorage) -> Vec<(String, &'static str, Vec<u8>)> {
        let mut out = Vec::new();
        for c in o.changes().await.unwrap() {
            let (path, kind) = match c {
                OverlayChange::Remove(p) => (p, "remove"),
                OverlayChange::Mkdir(p) => (p, "mkdir"),
                OverlayChange::Write(p) => (p, "write"),
            };
            let data = if kind == "write" {
                o.read(&path).await.unwrap()
            } else {
                vec![]
            };
            let rel = path.strip_prefix("/mnt/m/").unwrap().to_string();
            out.push((rel, kind, data));
        }
        out
    }

    #[tokio::test]
    async fn reads_pass_through_and_nothing_changes() {
        let o = overlay().await;
        assert_eq!(o.read("/mnt/m/a.txt").await.unwrap(), b"alpha");
        assert_eq!(names(&o, "/mnt/m").await, ["a.txt", "dir", "empty"]);
        assert!(changes(&o).await.is_empty());
    }

    #[tokio::test]
    async fn writes_copy_up_and_are_reported() {
        let o = overlay().await;
        // A partial write copies the file up first
        o.write_at("/mnt/m/a.txt", 0, b"A").await.unwrap();
        assert_eq!(o.read("/mnt/m/a.txt").await.unwrap(), b"Alpha");
        // Appending to a lower file in a lower directory
        o.append("/mnt/m/dir/b.txt", b"!").await.unwrap();
        // A new file in a new directory
        o.mkdir("/mnt/m/new").await.unwrap();
        o.write("/mnt/m/new/c.txt", b"gamma").await.unwrap();
        // Truncating to nothing needs no copy
        o.set_size("/mnt/m/dir/b.txt", 0).await.unwrap();
        assert_eq!(names(&o, "/mnt/m").await, ["a.txt", "dir", "empty", "new"]);
        assert_eq!(
            changes(&o).await,
            [
                ("new".into(), "mkdir", vec![]),
                ("a.txt".into(), "write", b"Alpha".to_vec()),
                ("dir/b.txt".into(), "write", vec![]),
                ("new/c.txt".into(), "write", b"gamma".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn deletes_are_whiteouts() {
        let o = overlay().await;
        o.delete("/mnt/m/dir/b.txt").await.unwrap();
        assert!(!o.exists("/mnt/m/dir/b.txt").await.unwrap());
        assert!(names(&o, "/mnt/m/dir").await.is_empty());
        o.rmdir("/mnt/m/dir").await.unwrap();
        assert!(matches!(
            o.rmdir("/mnt/m/nope").await,
            Err(VfsError::NotFound(_))
        ));
        // Remade, it's empty: the lower entries stay deleted
        o.mkdir("/mnt/m/dir").await.unwrap();
        assert!(names(&o, "/mnt/m/dir").await.is_empty());
        assert_eq!(
            changes(&o).await,
            [
                ("dir/b.txt".into(), "remove", vec![]),
                ("dir".into(), "remove", vec![]),
                ("dir".into(), "mkdir", vec![]),
            ]
        );
    }

    #[tokio::test]
    async fn rmdir_needs_the_merged_directory_empty() {
        let o = overlay().await;
        assert!(matches!(
            o.rmdir("/mnt/m/dir").await,
            Err(VfsError::DirectoryNotEmpty(_))
        ));
        o.rmdir("/mnt/m/empty").await.unwrap();
        assert_eq!(names(&o, "/mnt/m").await, ["a.txt", "dir"]);
    }

    #[tokio::test]
    async fn renames() {
        let o = overlay().await;
        // A lower file moves: copied up, old name whited out
        o.rename("/mnt/m/a.txt", "/mnt/m/dir/a2.txt").await.unwrap();
        assert!(!o.exists("/mnt/m/a.txt").await.unwrap());
        assert_eq!(o.read("/mnt/m/dir/a2.txt").await.unwrap(), b"alpha");
        // A lower directory can't
        assert!(matches!(
            o.rename("/mnt/m/dir", "/mnt/m/dir2").await,
            Err(VfsError::CrossDevice(_))
        ));
        // One made in the overlay can
        o.mkdir("/mnt/m/new").await.unwrap();
        o.write("/mnt/m/new/c.txt", b"c").await.unwrap();
        o.rename("/mnt/m/new", "/mnt/m/moved").await.unwrap();
        assert_eq!(o.read("/mnt/m/moved/c.txt").await.unwrap(), b"c");
        assert_eq!(
            changes(&o).await,
            [
                ("a.txt".into(), "remove", vec![]),
                ("moved".into(), "mkdir", vec![]),
                ("dir/a2.txt".into(), "write", b"alpha".to_vec()),
                ("moved/c.txt".into(), "write", b"c".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn directory_renames_respect_the_destination() {
        let o = overlay().await;
        o.mkdir("/mnt/m/new").await.unwrap();
        assert!(matches!(
            o.rename("/mnt/m/new", "/mnt/m/dir").await,
            Err(VfsError::DirectoryNotEmpty(_))
        ));
        assert!(matches!(
            o.rename("/mnt/m/new", "/mnt/m/a.txt").await,
            Err(VfsError::NotDirectory(_))
        ));
        // Onto an empty lower directory: it goes, and the new one is made
        o.rename("/mnt/m/new", "/mnt/m/empty").await.unwrap();
        assert_eq!(names(&o, "/mnt/m").await, ["a.txt", "dir", "empty"]);
        assert_eq!(
            changes(&o).await,
            [
                ("empty".into(), "remove", vec![]),
                ("empty".into(), "mkdir", vec![]),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_rename_copies_nothing() {
        let o = overlay().await;
        assert!(o.rename("/mnt/m/a.txt", "/mnt/m/missing/a").await.is_err());
        assert!(changes(&o).await.is_empty());
    }

    /// Two writers to one lower file: both writes land
    #[tokio::test]
    async fn concurrent_copy_ups_keep_both_writes() {
        let o = Arc::new(overlay().await);
        let (a, b) = (Arc::clone(&o), Arc::clone(&o));
        let (ra, rb) = tokio::join!(
            async move { a.append("/mnt/m/a.txt", b"1").await },
            async move { b.append("/mnt/m/a.txt", b"2").await },
        );
        ra.unwrap();
        rb.unwrap();
        let data = o.read("/mnt/m/a.txt").await.unwrap();
        assert!(data == b"alpha12" || data == b"alpha21", "{data:?}");
    }

    /// A lower directory's listing error isn't hidden by its upper copy
    #[tokio::test]
    async fn lower_listing_errors_come_through() {
        struct Failing;
        #[async_trait]
        impl VfsStorage for Failing {
            async fn read(&self, p: &str) -> VfsResult<Vec<u8>> {
                Err(VfsError::NotFound(p.into()))
            }
            async fn read_at(&self, p: &str, _: u64, _: u64) -> VfsResult<Vec<u8>> {
                Err(VfsError::NotFound(p.into()))
            }
            async fn write(&self, p: &str, _: &[u8]) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn write_at(&self, p: &str, _: u64, _: &[u8]) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn set_size(&self, p: &str, _: u64) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn delete(&self, p: &str) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn exists(&self, _: &str) -> VfsResult<bool> {
                Ok(true)
            }
            async fn list(&self, p: &str) -> VfsResult<Vec<DirEntry>> {
                Err(VfsError::Io(format!("{p}: the lower storage refused")))
            }
            async fn stat(&self, _: &str) -> VfsResult<Metadata> {
                Ok(Metadata {
                    is_dir: true,
                    ..Metadata::default()
                })
            }
            async fn mkdir(&self, p: &str) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn rmdir(&self, p: &str) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
            async fn rename(&self, p: &str, _: &str) -> VfsResult<()> {
                Err(VfsError::PermissionDenied(p.into()))
            }
        }
        let upper = Arc::new(InMemoryStorage::new());
        let o = OverlayStorage::new("/mnt/m", Arc::new(Failing), upper).unwrap();
        // The root is in the upper layer, but is the lower one's
        assert!(matches!(o.list("/mnt/m").await, Err(VfsError::Io(_))));
        assert!(o.rmdir("/mnt/m").await.is_err());
    }

    #[tokio::test]
    async fn create_file_sees_lower_files() {
        let o = overlay().await;
        assert!(matches!(
            o.create_file("/mnt/m/a.txt", true).await,
            Err(VfsError::AlreadyExists(_))
        ));
        assert!(!o.create_file("/mnt/m/a.txt", false).await.unwrap());
        assert!(o.create_file("/mnt/m/dir/new", true).await.unwrap());
        assert!(matches!(
            o.create_file("/mnt/m/missing/x", false).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_upper_layer_is_bounded() {
        let lower = InMemoryStorage::new();
        lower.mkdir_sync("/mnt/m").unwrap();
        lower.write("/mnt/m/big", &[0; 4096]).await.unwrap();
        let o = new(lower, 1024);
        assert!(matches!(
            o.write_at("/mnt/m/big", 0, b"x").await,
            Err(VfsError::QuotaExceeded(_))
        ));
        assert!(matches!(
            o.write("/mnt/m/new", &[0; 4096]).await,
            Err(VfsError::QuotaExceeded(_))
        ));
    }

    /// One lower storage under two overlays: each sees only its own writes,
    /// and the lower one none
    #[tokio::test]
    async fn a_shared_lower_layer_keeps_overlays_apart() {
        let lower = InMemoryStorage::new();
        lower.mkdir_sync("/mnt/m").unwrap();
        lower.write("/mnt/m/a.txt", b"alpha").await.unwrap();
        let lower: Arc<dyn VfsStorage> = Arc::new(lower);
        let fresh = || {
            let upper = Arc::new(InMemoryStorage::new());
            OverlayStorage::new("/mnt/m", Arc::clone(&lower), upper).unwrap()
        };
        let (one, two) = (fresh(), fresh());
        one.write("/mnt/m/a.txt", b"one").await.unwrap();
        two.delete("/mnt/m/a.txt").await.unwrap();
        assert_eq!(one.read("/mnt/m/a.txt").await.unwrap(), b"one");
        assert!(!two.exists("/mnt/m/a.txt").await.unwrap());
        assert_eq!(lower.read("/mnt/m/a.txt").await.unwrap(), b"alpha");
        assert_eq!(
            fresh().read("/mnt/m/a.txt").await.unwrap(),
            b"alpha".to_vec()
        );
    }
}
