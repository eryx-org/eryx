//! Copy-on-write layer for a writable mount (`Mount.writable`).
//!
//! The client's files stay as they are: the run's writes go to an in-memory
//! upper layer on the server, and deletions are recorded as whiteouts over
//! the lower layer (the client's [`RemoteStorage`](crate::mounts)). Reads see
//! the upper layer first, then the lower one minus whiteouts. When the run
//! ends, [`Overlay::changes`] lists what it did, for the client to apply.
//!
//! The rules, overlayfs-like but simpler:
//! - Writing part of a client file first copies it up whole; replacing it,
//!   or truncating it to nothing, doesn't.
//! - A deleted client file or directory becomes a whiteout. `rmdir` needs the
//!   merged directory to be empty, so a removed directory's client entries
//!   are all whited out already, and recreating it doesn't bring them back.
//! - Renaming a file copies it up and whites out the old name. Renaming a
//!   directory the run created itself is a plain rename; renaming a client
//!   directory is `EXDEV` (code that moves trees copies instead).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use eryx::vfs::{DirEntry, InMemoryStorage, Metadata, VfsError, VfsResult, VfsStorage};

use crate::proto::eryx::v1::{FsChange, FsMkdir, FsRemove, FsWrite, fs_change};

/// The largest `FsWrite` chunk of a changed file
pub const CHUNK: usize = 1024 * 1024;

/// One writable mount: the client's files under the run's changes
pub struct Overlay {
    /// The mount's guest path
    mount: String,
    lower: Arc<dyn VfsStorage>,
    /// The run's changes, shared by its writable mounts (paths are guest
    /// paths), so its limit is for the run
    upper: Arc<InMemoryStorage>,
    /// Most bytes the upper layer may hold, also checked before a copy-up
    max_bytes: u64,
    state: Mutex<State>,
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay")
            .field("mount", &self.mount)
            .finish()
    }
}

/// What the run did besides writing files, by guest path
#[derive(Default)]
struct State {
    /// Client entries the run deleted
    whiteouts: BTreeSet<String>,
    /// Directories the run made (the upper layer also has its parents, to
    /// hold its files, which aren't changes)
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

impl Overlay {
    /// An overlay over `lower`, mounted at `mount`, keeping its changes in
    /// `upper`, which holds at most `max_bytes`.
    ///
    /// # Errors
    ///
    /// If the upper layer can't hold the mount's directory.
    pub fn new(
        mount: &str,
        lower: Arc<dyn VfsStorage>,
        upper: Arc<InMemoryStorage>,
        max_bytes: u64,
    ) -> VfsResult<Self> {
        upper.mkdir_sync(mount)?;
        Ok(Self {
            mount: mount.to_string(),
            lower,
            upper,
            max_bytes,
            state: Mutex::default(),
        })
    }

    /// `path` relative to the mount
    fn relative<'a>(&self, path: &'a str) -> &'a str {
        path.strip_prefix(self.mount.as_str())
            .unwrap_or(path)
            .trim_start_matches('/')
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
        if parent.len() <= self.mount.len() {
            return Ok(());
        }
        let mut dir = self.mount.clone();
        for component in parent[self.mount.len()..]
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

    /// Have `path` in the upper layer, copying a client file up whole
    async fn copy_up(&self, path: &str) -> VfsResult<()> {
        match self.find(path).await? {
            Found::Upper(m) | Found::Lower(m) if m.is_dir => {
                Err(VfsError::NotFile(path.to_string()))
            }
            Found::Upper(_) => Ok(()),
            Found::Lower(m) => {
                if m.size > self.max_bytes {
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

    /// The merged listing of the directory `path`
    async fn merged(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let found = self.find(path).await?;
        match found.metadata() {
            None => return Err(VfsError::NotFound(path.to_string())),
            Some(m) if !m.is_dir => return Err(VfsError::NotDirectory(path.to_string())),
            Some(_) => {}
        }
        let mut entries = std::collections::BTreeMap::new();
        // A directory only in the upper layer (the run made it, or remade
        // it) has no client entries left to show
        let lower = match self.lower.list(path).await {
            Ok(lower) => lower,
            Err(VfsError::NotFound(_) | VfsError::NotDirectory(_)) => Vec::new(),
            Err(e) if matches!(found, Found::Upper(_)) => {
                tracing::debug!(path, error = %e, "listing the client's directory failed");
                Vec::new()
            }
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

    /// What the run changed, in an order the client can apply as it goes:
    /// removals deepest first, then new directories parents first, then
    /// each file's whole contents in [`CHUNK`]s (the first at offset 0,
    /// which creates or truncates it).
    ///
    /// # Errors
    ///
    /// If the upper layer can't be read.
    pub async fn changes(&self) -> VfsResult<Vec<FsChange>> {
        let (whiteouts, created) = {
            let state = lock(&self.state);
            (state.whiteouts.clone(), state.created.clone())
        };
        let change = |path: &str, change| FsChange {
            mount: self.mount.clone(),
            path: self.relative(path).to_string(),
            change: Some(change),
        };
        let mut removed: Vec<&String> = whiteouts.iter().collect();
        removed.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
        let mut changes: Vec<FsChange> = removed
            .into_iter()
            .map(|p| change(p, fs_change::Change::Remove(FsRemove {})))
            .collect();
        for dir in &created {
            if matches!(self.upper.stat(dir).await, Ok(m) if m.is_dir) {
                changes.push(change(dir, fs_change::Change::Mkdir(FsMkdir {})));
            }
        }
        // Every file in the upper layer was written (or copied up to be)
        let mut dirs = vec![self.mount.clone()];
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
        for path in files {
            let data = self.upper.read(&path).await?;
            let mut offset = 0;
            for chunk in data.chunks(CHUNK) {
                let write = FsWrite {
                    offset: offset as u64,
                    data: chunk.to_vec(),
                };
                changes.push(change(&path, fs_change::Change::Write(write)));
                offset += chunk.len();
            }
            if data.is_empty() {
                let write = FsWrite::default();
                changes.push(change(&path, fs_change::Change::Write(write)));
            }
        }
        Ok(changes)
    }
}

#[async_trait]
impl VfsStorage for Overlay {
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
        // Truncating a client file to nothing (`open(..., 'w')`) needn't
        // fetch it first
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
        // Whatever the client has there goes too
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
            // Only a directory wholly the run's moves as one
            if !lock(&self.state).created.contains(from) || self.lower_has(from).await? {
                return Err(VfsError::CrossDevice(format!(
                    "{from}: a directory from the client can't be renamed, only copied"
                )));
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
        self.copy_up(from).await?;
        self.ensure_parent(to).await?;
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
        // A client directory's times aren't the run's to change, and aren't
        // reported, so they're left alone
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

impl Overlay {
    /// Whether the client has something at `path` (deleted or not)
    async fn lower_has(&self, path: &str) -> VfsResult<bool> {
        match self.lower.stat(path).await {
            Ok(_) => Ok(true),
            Err(VfsError::NotFound(_) | VfsError::NotDirectory(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn new(lower: InMemoryStorage, max_bytes: u64) -> Overlay {
        let upper = Arc::new(InMemoryStorage::with_max_bytes(max_bytes));
        Overlay::new("/mnt/m", Arc::new(lower), upper, max_bytes).unwrap()
    }

    /// An overlay at `/mnt/m` over a client tree with `a.txt`, `dir/b.txt`
    /// and the empty `empty/`
    async fn overlay() -> Overlay {
        let lower = InMemoryStorage::new();
        lower.mkdir_sync("/mnt/m/dir").unwrap();
        lower.mkdir_sync("/mnt/m/empty").unwrap();
        lower.write("/mnt/m/a.txt", b"alpha").await.unwrap();
        lower.write("/mnt/m/dir/b.txt", b"beta").await.unwrap();
        new(lower, 1024 * 1024)
    }

    async fn names(o: &Overlay, path: &str) -> Vec<String> {
        o.list(path)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    /// The changes as `(path, kind, offset, data)`
    async fn changes(o: &Overlay) -> Vec<(String, &'static str, u64, Vec<u8>)> {
        o.changes()
            .await
            .unwrap()
            .into_iter()
            .map(|c| {
                assert_eq!(c.mount, "/mnt/m");
                match c.change.unwrap() {
                    fs_change::Change::Remove(_) => (c.path, "remove", 0, vec![]),
                    fs_change::Change::Mkdir(_) => (c.path, "mkdir", 0, vec![]),
                    fs_change::Change::Write(w) => (c.path, "write", w.offset, w.data),
                }
            })
            .collect()
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
        // Appending to a client file in a client directory
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
                ("new".into(), "mkdir", 0, vec![]),
                ("a.txt".into(), "write", 0, b"Alpha".to_vec()),
                ("dir/b.txt".into(), "write", 0, vec![]),
                ("new/c.txt".into(), "write", 0, b"gamma".to_vec()),
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
        // Remade, it's empty: the client's entries stay deleted
        o.mkdir("/mnt/m/dir").await.unwrap();
        assert!(names(&o, "/mnt/m/dir").await.is_empty());
        assert_eq!(
            changes(&o).await,
            [
                ("dir/b.txt".into(), "remove", 0, vec![]),
                ("dir".into(), "remove", 0, vec![]),
                ("dir".into(), "mkdir", 0, vec![]),
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
        // A client file moves: copied up, old name whited out
        o.rename("/mnt/m/a.txt", "/mnt/m/dir/a2.txt").await.unwrap();
        assert!(!o.exists("/mnt/m/a.txt").await.unwrap());
        assert_eq!(o.read("/mnt/m/dir/a2.txt").await.unwrap(), b"alpha");
        // A client directory can't
        assert!(matches!(
            o.rename("/mnt/m/dir", "/mnt/m/dir2").await,
            Err(VfsError::CrossDevice(_))
        ));
        // One the run made can
        o.mkdir("/mnt/m/new").await.unwrap();
        o.write("/mnt/m/new/c.txt", b"c").await.unwrap();
        o.rename("/mnt/m/new", "/mnt/m/moved").await.unwrap();
        assert_eq!(o.read("/mnt/m/moved/c.txt").await.unwrap(), b"c");
        assert_eq!(
            changes(&o).await,
            [
                ("a.txt".into(), "remove", 0, vec![]),
                ("moved".into(), "mkdir", 0, vec![]),
                ("dir/a2.txt".into(), "write", 0, b"alpha".to_vec()),
                ("moved/c.txt".into(), "write", 0, b"c".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn create_file_sees_client_files() {
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

    #[tokio::test]
    async fn big_files_come_in_chunks() {
        let o = new(InMemoryStorage::new(), 8 * 1024 * 1024);
        let data: Vec<u8> = (0..CHUNK * 2 + 10).map(|i| (i % 251) as u8).collect();
        o.write("/mnt/m/f", &data).await.unwrap();
        let changes = changes(&o).await;
        let offsets: Vec<u64> = changes.iter().map(|c| c.2).collect();
        assert_eq!(offsets, [0, CHUNK as u64, 2 * CHUNK as u64]);
        let joined: Vec<u8> = changes.into_iter().flat_map(|c| c.3).collect();
        assert_eq!(joined, data);
    }
}
