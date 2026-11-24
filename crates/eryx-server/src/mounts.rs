//! Read-only directories a client serves to its run (`ExecuteRequest.mounts`).
//!
//! Each mount is a `RemoteStorage` volume: the sandbox's file operations
//! beneath it become [`FsRequest`]s on the response stream, which the client
//! answers with [`FsResponse`]s. The client decides what each path serves, so
//! it's the security boundary, not us.
//!
//! Ported from eryx-run's `/v1/stream` mounts. Round trips are what's slow, so
//! a run caches what it learns: reads fetch aligned [`BLOCK_SIZE`] blocks, a
//! `list` brings each entry's metadata and small files' contents, and a `stat`
//! is answered from its parent's listing when there is one. The guest polls a
//! storage operation once, drops it and runs it again later (see
//! `VfsStorage`), so every request is a shared future kept in the cache, and
//! the rerun joins it instead of asking again.
//!
//! A writable mount (`Mount.writable`) is an [`OverlayStorage`] over its
//! `RemoteStorage`: the client's files stay as they are, the run's writes stay
//! on the server, and when the run ends [`Mounts::changes`] lists them for the
//! client to apply.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use eryx::vfs::{
    DirEntry, InMemoryStorage, Metadata, OverlayChange, OverlayStorage, VfsError, VfsResult,
    VfsStorage,
};
use eryx::{CancellationToken, VolumeMount};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use prost::Message as _;
use tokio::sync::{mpsc, oneshot};

/// A block of a file's data, shared by the reads that need it
type Bytes = Arc<Vec<u8>>;

use crate::proto::eryx::v1::{
    FsChange, FsErrorCode, FsList, FsMetadata, FsMkdir, FsRead, FsRemove, FsRequest, FsResponse,
    FsStat, FsWrite, Mount, ServerMessage, fs_change, fs_request, fs_response, server_message,
};

/// The largest `FsWrite` chunk of a changed file
pub const CHUNK: usize = 1024 * 1024;

/// Mounts go beneath this
const MOUNT_ROOT: &str = "/mnt/";
/// What a read that misses the cache fetches, aligned to it
// ponytail: fixed block size; adaptive readahead and prefetch when sequential
// reads of big files need fewer round trips
pub const BLOCK_SIZE: u64 = 256 * 1024;
/// Files up to this size are kept when they come with their directory's listing
pub const INLINE_MAX: u64 = 64 * 1024;
/// Most a read fetches at once, as the guest reads at most this much a time
const MAX_READ: u64 = 1024 * 1024;
/// What a listing's entry counts for in the cache, besides its name
const ENTRY_BYTES: u64 = 64;

/// Per-run limits on a request's mounts, set by the operator (the
/// `--max-mount*` flags). Not client-controllable: the cache and listings are
/// host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountLimits {
    /// Mounts one request may have.
    pub max_mounts: usize,
    /// Round trips to the client (`FsRequest`s) per run.
    pub max_ops: u32,
    /// Bytes of `FsResponse`s the client may send a run.
    pub max_transfer_bytes: usize,
    /// Host memory a run's cache of file data may hold, evicted oldest first.
    pub max_cache_bytes: u64,
    /// Host memory a run's directory listings may hold, kept for the run.
    pub max_listing_bytes: u64,
    /// Entries in one directory's listing.
    pub max_dir_entries: usize,
    /// Host memory a run's changes to its writable mounts may hold.
    pub max_write_bytes: u64,
}

impl Default for MountLimits {
    /// eryx-run's limits for its `/v1/stream` mounts.
    fn default() -> Self {
        Self {
            max_mounts: 8,
            max_ops: 1000,
            max_transfer_bytes: 256 * 1024 * 1024,
            max_cache_bytes: 16 * 1024 * 1024,
            max_listing_bytes: 16 * 1024 * 1024,
            max_dir_entries: 10_000,
            max_write_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Check a request's mounts: at most `max_mounts`, each a plain path under
/// `/mnt/` (no `.`, `..`, empty components or trailing slash), none inside
/// another.
///
/// # Errors
///
/// What's wrong with them, for the client.
pub fn check(mounts: &[Mount], max_mounts: usize) -> Result<(), String> {
    if mounts.len() > max_mounts {
        return Err(format!("a request can have at most {max_mounts} mounts"));
    }
    for (i, mount) in mounts.iter().enumerate() {
        let path = &mount.path;
        let plain = path
            .strip_prefix(MOUNT_ROOT)
            .is_some_and(|rest| rest.split('/').all(|c| !matches!(c, "" | "." | "..")));
        if !plain {
            return Err(format!(
                "mount path {path:?} must be a path under /mnt/, like /mnt/repo"
            ));
        }
        let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
        if let Some(other) = mounts[..i]
            .iter()
            .find(|m| inside(path, &m.path) || inside(&m.path, path))
        {
            return Err(format!("mount paths {:?} and {path:?} overlap", other.path));
        }
    }
    Ok(())
}

// ============================================================================
// Answers
// ============================================================================

/// A file's or directory's metadata, as cached for the run
#[derive(Debug, Clone)]
struct Stat {
    is_dir: bool,
    size: u64,
    /// Seconds since the Unix epoch
    modified: Option<f64>,
}

impl From<FsMetadata> for Stat {
    fn from(m: FsMetadata) -> Self {
        Self {
            is_dir: m.is_dir,
            size: if m.is_dir { 0 } else { m.size },
            modified: m.modified,
        }
    }
}

/// Why a remote operation failed, as cached for the run
#[derive(Debug, Clone)]
enum Failure {
    NotFound,
    NotDirectory,
    IsDirectory,
    PermissionDenied,
    Other(String),
}

impl Failure {
    fn into_vfs(self, path: &str) -> VfsError {
        let path = path.to_string();
        match self {
            Failure::NotFound => VfsError::NotFound(path),
            Failure::NotDirectory => VfsError::NotDirectory(path),
            Failure::IsDirectory => VfsError::NotFile(path),
            Failure::PermissionDenied => VfsError::PermissionDenied(path),
            Failure::Other(message) => VfsError::Io(format!("{path}: {message}")),
        }
    }
}

type Answer = Result<fs_response::Result, Failure>;
type Fetch<T> = Shared<BoxFuture<'static, Result<T, Failure>>>;

/// A fetch that has already finished. It's polled once here, since a shared
/// future only shows its output (`peek`, which eviction checks) once polled.
fn fetched<T: Clone + Send + Sync + 'static>(value: T) -> Fetch<T> {
    let fetch = futures::future::ready(Ok(value)).boxed().shared();
    let _ = fetch.clone().now_or_never();
    fetch
}

// ============================================================================
// The run's side of the stream
// ============================================================================

/// A run's mounts: what the sandbox mounts, and what the stream's reader hands
/// the client's answers to
pub struct Mounts {
    remote: Arc<Remote>,
    /// Each mount's path, and its overlay if it's writable
    mounts: Vec<(String, Option<Arc<OverlayStorage>>)>,
}

impl Mounts {
    /// The mounts for `defs` (already [`check`]ed). Their requests go out on
    /// `requests`, and fail at once after `closed`, when the client can no
    /// longer answer. The sender is weak, so the mounts never keep the
    /// response stream open.
    ///
    /// # Errors
    ///
    /// If a writable mount's overlay can't be made.
    pub fn new(
        defs: &[Mount],
        limits: MountLimits,
        requests: mpsc::WeakSender<ServerMessage>,
        closed: CancellationToken,
    ) -> VfsResult<Self> {
        let remote = Arc::new(Remote {
            limits,
            requests,
            closed,
            pending: Mutex::default(),
            ops: AtomicU32::new(0),
            received: AtomicUsize::new(0),
            cache: Mutex::new(Cache::new(limits.max_cache_bytes)),
        });
        // Made only for a request with writable mounts, as it charges their
        // directories to the limit
        let upper = defs
            .iter()
            .any(|d| d.writable)
            .then(|| Arc::new(InMemoryStorage::with_max_bytes(limits.max_write_bytes)));
        let mounts = defs
            .iter()
            .map(|def| {
                let overlay = match &upper {
                    Some(upper) if def.writable => {
                        let lower = Arc::new(RemoteStorage {
                            mount: def.path.clone(),
                            remote: Arc::clone(&remote),
                        });
                        let upper = Arc::clone(upper);
                        let overlay = OverlayStorage::new(&def.path, lower, upper);
                        Some(Arc::new(overlay?))
                    }
                    _ => None,
                };
                Ok((def.path.clone(), overlay))
            })
            .collect::<VfsResult<_>>()?;
        Ok(Self { remote, mounts })
    }

    /// One volume per mount: read-only, or its overlay if it's writable
    pub fn volumes(&self) -> Vec<VolumeMount> {
        self.mounts
            .iter()
            .map(|(path, overlay)| match overlay {
                Some(overlay) => {
                    VolumeMount::storage(path.clone(), Arc::clone(overlay) as Arc<dyn VfsStorage>)
                }
                None => {
                    let storage = RemoteStorage {
                        mount: path.clone(),
                        remote: Arc::clone(&self.remote),
                    };
                    VolumeMount::read_only_storage(path.clone(), Arc::new(storage))
                }
            })
            .collect()
    }

    /// What the run changed in its writable mounts, in the order to apply
    /// them (see [`OverlayStorage::changes`]), each file's whole contents in
    /// [`CHUNK`]s (the first at offset 0, which creates or truncates it)
    ///
    /// # Errors
    ///
    /// If an overlay's changes can't be read.
    pub async fn changes(&self) -> VfsResult<Vec<FsChange>> {
        let mut changes = Vec::new();
        for (mount, overlay) in &self.mounts {
            let Some(overlay) = overlay else { continue };
            let change = |path: &str, change| FsChange {
                mount: mount.clone(),
                path: path
                    .strip_prefix(mount.as_str())
                    .unwrap_or(path)
                    .trim_start_matches('/')
                    .to_string(),
                change: Some(change),
            };
            for c in overlay.changes().await? {
                match c {
                    OverlayChange::Remove(p) => {
                        changes.push(change(&p, fs_change::Change::Remove(FsRemove {})));
                    }
                    OverlayChange::Mkdir(p) => {
                        changes.push(change(&p, fs_change::Change::Mkdir(FsMkdir {})));
                    }
                    OverlayChange::Write(p) => {
                        let data = overlay.read(&p).await?;
                        if data.is_empty() {
                            let write = FsWrite::default();
                            changes.push(change(&p, fs_change::Change::Write(write)));
                        }
                        for (i, chunk) in data.chunks(CHUNK).enumerate() {
                            let write = FsWrite {
                                offset: (i * CHUNK) as u64,
                                data: chunk.to_vec(),
                            };
                            changes.push(change(&p, fs_change::Change::Write(write)));
                        }
                    }
                    // OverlayChange is non_exhaustive; a new kind needs a
                    // proto message before it can be sent
                    other => {
                        return Err(VfsError::Io(format!("an unsupported change: {other:?}")));
                    }
                }
            }
        }
        Ok(changes)
    }

    /// Take the client's answer to an operation
    pub fn reply(&self, response: FsResponse) {
        // An answer to an operation we didn't ask for, or one the run has
        // given up on, is dropped, and doesn't count towards the transfer
        let Some(tx) = lock(&self.remote.pending).remove(&response.op_id) else {
            tracing::debug!(op_id = %response.op_id, "fs_response for no pending operation");
            return;
        };
        let len = response.encoded_len();
        let received = self.remote.received.fetch_add(len, Ordering::Relaxed) + len;
        let max = self.remote.limits.max_transfer_bytes;
        let answer = if received > max {
            Err(Failure::Other(format!(
                "the client sent more than {} MiB",
                max >> 20
            )))
        } else {
            response
                .result
                .ok_or_else(|| Failure::Other("an empty fs_response".into()))
        };
        let _ = tx.send(answer);
    }

    /// When the run is done (it finished, or the client went), operations
    /// still waiting for an answer fail rather than wait out the run's
    /// timeout. The cache goes too: an unfinished fetch in it holds the
    /// `Remote` that holds the cache, which would never be freed.
    pub fn close(&self) {
        lock(&self.remote.pending).clear();
        *lock(&self.remote.cache) = Cache::new(self.remote.limits.max_cache_bytes);
    }
}

impl std::fmt::Debug for Mounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<_> = self.mounts.iter().map(|(p, o)| (p, o.is_some())).collect();
        f.debug_struct("Mounts").field("mounts", &paths).finish()
    }
}

impl Drop for Mounts {
    fn drop(&mut self) {
        self.close();
    }
}

/// A poisoned lock only means another operation panicked; the maps are
/// still consistent
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a run's mounts share: the way to the client, and the cache
struct Remote {
    limits: MountLimits,
    requests: mpsc::WeakSender<ServerMessage>,
    closed: CancellationToken,
    pending: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
    /// Round trips so far
    ops: AtomicU32,
    /// Bytes of `FsResponse`s the client has sent
    received: AtomicUsize,
    cache: Mutex<Cache>,
}

/// What a run has learned from the client, by guest path
struct Cache {
    /// Most bytes of blocks it may hold
    max_bytes: u64,
    stats: HashMap<String, Fetch<Stat>>,
    lists: HashMap<String, Fetch<Arc<BTreeMap<String, Stat>>>>,
    blocks: HashMap<(String, u64), Fetch<Bytes>>,
    /// Blocks in the order they were added, with their sizes, for eviction
    order: VecDeque<((String, u64), u64)>,
    /// Bytes of blocks held
    used: u64,
    /// Bytes of listings held
    listed: u64,
}

impl Cache {
    fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            stats: HashMap::new(),
            lists: HashMap::new(),
            blocks: HashMap::new(),
            order: VecDeque::new(),
            used: 0,
            listed: 0,
        }
    }

    /// Make room for `bytes`, evicting finished blocks oldest first
    fn reserve(&mut self, bytes: u64) -> Result<(), Failure> {
        // Blocks still being fetched go back in the queue: a rerun of their
        // read joins them
        for _ in 0..self.order.len() {
            if self.used + bytes <= self.max_bytes {
                break;
            }
            let Some((key, size)) = self.order.pop_front() else {
                break;
            };
            if self.blocks.get(&key).is_some_and(|b| b.peek().is_none()) {
                self.order.push_back((key, size));
            } else {
                self.blocks.remove(&key);
                self.used -= size;
            }
        }
        if self.used + bytes > self.max_bytes {
            return Err(Failure::Other(format!(
                "the mounts' cache of {} MiB is full",
                self.max_bytes >> 20
            )));
        }
        self.used += bytes;
        Ok(())
    }

    fn add_block(&mut self, key: (String, u64), size: u64, fetch: Fetch<Bytes>) {
        self.order.push_back((key.clone(), size));
        self.blocks.insert(key, fetch);
    }
}

impl Remote {
    /// Why a new operation would fail without asking: the client can't
    /// answer, or the run has used up its operations. Checked before a new
    /// entry goes in the cache, so failures past the limit aren't kept, and
    /// the cache holds at most about `max_ops` entries besides blocks.
    fn refusal(&self) -> Option<Failure> {
        if self.closed.is_cancelled() {
            Some(gone())
        } else if self.ops.load(Ordering::Relaxed) >= self.limits.max_ops {
            Some(too_many_ops(self.limits.max_ops))
        } else {
            None
        }
    }

    /// Ask the client for `op` on `path` (relative to `mount`) and wait for
    /// its answer
    fn request(
        self: &Arc<Self>,
        mount: &str,
        path: &str,
        op: fs_request::Op,
    ) -> BoxFuture<'static, Answer> {
        let remote = Arc::clone(self);
        let (mount, path) = (mount.to_string(), path.to_string());
        async move {
            if remote.closed.is_cancelled() {
                return Err(gone());
            }
            let max_ops = remote.limits.max_ops;
            if remote.ops.fetch_add(1, Ordering::Relaxed) >= max_ops {
                return Err(too_many_ops(max_ops));
            }
            let requests = remote.requests.upgrade().ok_or_else(gone)?;
            let op_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            lock(&remote.pending).insert(op_id.clone(), tx);
            let message = ServerMessage {
                message: Some(server_message::Message::FsRequest(FsRequest {
                    op_id: op_id.clone(),
                    mount,
                    path,
                    op: Some(op),
                })),
            };
            if requests.send(message).await.is_err() {
                lock(&remote.pending).remove(&op_id);
                return Err(gone());
            }
            drop(requests);
            let answer = tokio::select! {
                answer = rx => answer.map_err(|_| gone())??,
                () = remote.closed.cancelled() => return Err(gone()),
            };
            match answer {
                fs_response::Result::Error(error) => Err(match error.code() {
                    FsErrorCode::NotFound => Failure::NotFound,
                    FsErrorCode::NotADirectory => Failure::NotDirectory,
                    FsErrorCode::IsADirectory => Failure::IsDirectory,
                    FsErrorCode::PermissionDenied => Failure::PermissionDenied,
                    FsErrorCode::Other => Failure::Other(error.message),
                }),
                answer => Ok(answer),
            }
        }
        .boxed()
    }
}

fn gone() -> Failure {
    Failure::Other("the client can no longer answer".to_string())
}

fn too_many_ops(max: u32) -> Failure {
    Failure::Other(format!(
        "a run can make at most {max} file operations on its mounts"
    ))
}

/// The answer wasn't what the operation asked for
fn wrong_answer(wanted: &str) -> Failure {
    Failure::Other(format!("the answer has no {wanted}"))
}

// ============================================================================
// Storage
// ============================================================================

/// One mount's storage: asks the client, through its run's cache
struct RemoteStorage {
    /// The mount's guest path
    mount: String,
    remote: Arc<Remote>,
}

impl RemoteStorage {
    /// `path` relative to the mount, refusing `.` and `..` (eryx resolves
    /// them first, but the client shouldn't have to trust that)
    fn relative<'a>(&self, path: &'a str) -> VfsResult<&'a str> {
        let rest = path
            .strip_prefix(self.mount.as_str())
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .ok_or_else(|| VfsError::InvalidPath(path.to_string()))?
            .trim_matches('/');
        if rest.split('/').any(|c| c == "." || c == "..") {
            return Err(VfsError::InvalidPath(path.to_string()));
        }
        Ok(rest)
    }

    /// `path`'s directory listing, fetched once per run
    async fn listing(&self, path: &str) -> VfsResult<Arc<BTreeMap<String, Stat>>> {
        let rel = self.relative(path)?;
        let key = self.key(rel);
        let fetch = {
            let mut cache = lock(&self.remote.cache);
            match cache.lists.get(&key) {
                Some(fetch) => fetch.clone(),
                None => {
                    if let Some(failure) = self.remote.refusal() {
                        return Err(failure.into_vfs(path));
                    }
                    let fetch = self.fetch_listing(&key, rel);
                    cache.lists.insert(key, fetch.clone());
                    fetch
                }
            }
        };
        fetch.await.map_err(|f| f.into_vfs(path))
    }

    fn fetch_listing(&self, key: &str, rel: &str) -> Fetch<Arc<BTreeMap<String, Stat>>> {
        let request = self
            .remote
            .request(&self.mount, rel, fs_request::Op::List(FsList {}));
        let remote = Arc::clone(&self.remote);
        let dir = key.to_string();
        async move {
            let fs_response::Result::Listing(listing) = request.await? else {
                return Err(wrong_answer("listing"));
            };
            let entries = listing.entries;
            let limits = remote.limits;
            if entries.len() > limits.max_dir_entries {
                return Err(Failure::Other(format!(
                    "a directory can have at most {} entries",
                    limits.max_dir_entries
                )));
            }
            if let Some(entry) = entries.iter().find(|e| {
                e.name.is_empty() || e.name.contains('/') || e.name == "." || e.name == ".."
            }) {
                return Err(Failure::Other(format!("bad entry name {:?}", entry.name)));
            }
            let mut listing = BTreeMap::new();
            let mut cache = lock(&remote.cache);
            let names: u64 = entries.iter().map(|e| e.name.len() as u64).sum();
            let bytes = names + ENTRY_BYTES * entries.len() as u64;
            if cache.listed + bytes > limits.max_listing_bytes {
                return Err(Failure::Other(format!(
                    "the mounts' listings can hold at most {} MiB",
                    limits.max_listing_bytes >> 20
                )));
            }
            cache.listed += bytes;
            for entry in entries {
                let stat = Stat::from(entry.metadata.unwrap_or_default());
                if let Some(content) = entry.content
                    && !stat.is_dir
                {
                    if content.len() as u64 != stat.size {
                        return Err(Failure::Other(format!(
                            "{:?} has {} bytes of content, not its size of {}",
                            entry.name,
                            content.len(),
                            stat.size
                        )));
                    }
                    // Small files fit in their first block (bigger ones are
                    // read in blocks); without room they're fetched when read
                    let key = (format!("{dir}/{}", entry.name), 0);
                    let size = stat.size;
                    if size <= INLINE_MAX.min(BLOCK_SIZE)
                        && !cache.blocks.contains_key(&key)
                        && cache.reserve(size).is_ok()
                    {
                        cache.add_block(key, size, fetched(Arc::new(content)));
                    }
                }
                listing.insert(entry.name, stat);
            }
            Ok(Arc::new(listing))
        }
        .boxed()
        .shared()
    }

    /// The cache's key for `rel`: its guest path, normalised
    fn key(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.mount.clone()
        } else {
            format!("{}/{rel}", self.mount)
        }
    }

    async fn stat_remote(&self, path: &str) -> VfsResult<Stat> {
        let rel = self.relative(path)?;
        let key = self.key(rel);
        let fetch = {
            let mut cache = lock(&self.remote.cache);
            // A finished listing of the parent answers without a round trip
            if let Some((parent, name)) = key.rsplit_once('/')
                && let Some(Ok(listing)) = cache.lists.get(parent).and_then(|l| l.peek())
            {
                return listing
                    .get(name)
                    .cloned()
                    .ok_or_else(|| VfsError::NotFound(path.to_string()));
            }
            if !cache.stats.contains_key(&key)
                && let Some(failure) = self.remote.refusal()
            {
                return Err(failure.into_vfs(path));
            }
            let stat = cache.stats.entry(key).or_insert_with(|| {
                let request =
                    self.remote
                        .request(&self.mount, rel, fs_request::Op::Stat(FsStat {}));
                async move {
                    match request.await? {
                        fs_response::Result::Stat(stat) => Ok(Stat::from(stat)),
                        _ => Err(wrong_answer("stat")),
                    }
                }
                .boxed()
                .shared()
            });
            stat.clone()
        };
        fetch.await.map_err(|f| f.into_vfs(path))
    }

    /// Block `index` of the file at `rel`, `size` bytes long, from the cache
    /// or the client
    fn block(&self, rel: &str, index: u64, size: u64) -> Result<Fetch<Bytes>, Failure> {
        let key = (self.key(rel), index);
        let mut cache = lock(&self.remote.cache);
        if let Some(fetch) = cache.blocks.get(&key) {
            return Ok(fetch.clone());
        }
        if let Some(failure) = self.remote.refusal() {
            return Err(failure);
        }
        cache.reserve(size)?;
        let op = fs_request::Op::Read(FsRead {
            offset: index * BLOCK_SIZE,
            len: size,
        });
        let request = self.remote.request(&self.mount, rel, op);
        let fetch = async move {
            let fs_response::Result::Data(data) = request.await? else {
                return Err(wrong_answer("data"));
            };
            // The cache counts what was asked for, so it can't hold more
            if data.len() as u64 > size {
                return Err(Failure::Other(format!(
                    "sent {} bytes for a read of {size}",
                    data.len()
                )));
            }
            Ok(Arc::new(data))
        }
        .boxed()
        .shared();
        cache.add_block(key, size, fetch.clone());
        Ok(fetch)
    }
}

fn metadata(stat: &Stat) -> Metadata {
    let modified = stat
        .modified
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .and_then(|d| SystemTime::UNIX_EPOCH.checked_add(d))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Metadata {
        is_dir: stat.is_dir,
        size: stat.size,
        created: modified,
        modified,
        accessed: modified,
    }
}

fn read_only(path: &str) -> VfsError {
    VfsError::PermissionDenied(format!("{path}: the mount is read-only"))
}

#[async_trait]
impl VfsStorage for RemoteStorage {
    async fn read(&self, path: &str) -> VfsResult<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let chunk = self.read_at(path, data.len() as u64, MAX_READ).await?;
            if chunk.is_empty() {
                return Ok(data);
            }
            data.extend_from_slice(&chunk);
        }
    }

    /// Reads the blocks covering the range, up to the size the file had when
    /// it was first stat'd in this run, and at most [`MAX_READ`]
    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>> {
        let file = self.stat_remote(path).await?;
        if file.is_dir {
            return Err(VfsError::NotFile(path.to_string()));
        }
        let end = offset.saturating_add(len.min(MAX_READ)).min(file.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        let rel = self.relative(path)?;
        let first = offset / BLOCK_SIZE;
        let blocks = (first..=(end - 1) / BLOCK_SIZE)
            .map(|i| self.block(rel, i, BLOCK_SIZE.min(file.size - i * BLOCK_SIZE)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|f| f.into_vfs(path))?;
        let blocks = futures::future::try_join_all(blocks)
            .await
            .map_err(|f| f.into_vfs(path))?;
        let mut data = Vec::new();
        for (block_start, block) in (first..).map(|i| i * BLOCK_SIZE).zip(&blocks) {
            // The part of this block in the range, both ends under BLOCK_SIZE
            let wanted = offset.max(block_start) - block_start..(end - block_start).min(BLOCK_SIZE);
            let index = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
            let got = index(wanted.start)..index(wanted.end).min(block.len());
            data.extend_from_slice(block.get(got).unwrap_or_default());
            // A short block means the file shrank on the client
            if (block.len() as u64) < wanted.end {
                break;
            }
        }
        Ok(data)
    }

    async fn write(&self, path: &str, _data: &[u8]) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn write_at(&self, path: &str, _offset: u64, _data: &[u8]) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn set_size(&self, path: &str, _size: u64) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn delete(&self, path: &str) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn exists(&self, path: &str) -> VfsResult<bool> {
        match self.stat_remote(path).await {
            Ok(_) => Ok(true),
            Err(VfsError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let listing = self.listing(path).await?;
        Ok(listing
            .iter()
            .map(|(name, stat)| DirEntry {
                name: name.clone(),
                metadata: metadata(stat),
            })
            .collect())
    }

    async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        Ok(metadata(&self.stat_remote(path).await?))
    }

    async fn mkdir(&self, path: &str) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn rmdir(&self, path: &str) -> VfsResult<()> {
        Err(read_only(path))
    }

    async fn rename(&self, from: &str, _to: &str) -> VfsResult<()> {
        Err(read_only(from))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::proto::eryx::v1::{FsEntry, FsError, FsListing};

    fn defs(paths: &[&str]) -> Vec<Mount> {
        paths
            .iter()
            .map(|p| Mount {
                path: (*p).to_string(),
                ..Mount::default()
            })
            .collect()
    }

    #[test]
    fn mount_paths_are_plain_and_apart() {
        let check = |m: &[Mount]| check(m, 8);
        assert!(check(&defs(&["/mnt/repo", "/mnt/data/set"])).is_ok());
        for bad in [
            &["/data/x"][..],
            &["/mnt"],
            &["/mnt/"],
            &["/mnt/repo/"],
            &["/mnt/a//b"],
            &["/mnt/../etc"],
            &["/mnt/./a"],
            &["/mnt/a", "/mnt/a"],
            &["/mnt/a", "/mnt/a/b"],
            &["/mnt/a/b", "/mnt/a"],
        ] {
            assert!(check(&defs(bad)).is_err(), "{bad:?}");
        }
        assert!(check(&defs(&["/mnt/ab", "/mnt/a"])).is_ok());
        let many: Vec<String> = (0..=8).map(|i| format!("/mnt/{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(check(&defs(&many)).is_err());
    }

    /// One mount at `/mnt/m` with its stream: the storage, its run's mounts
    /// (to answer with), the requests, and the token that closes them
    fn mount() -> (
        RemoteStorage,
        Arc<Mounts>,
        mpsc::Receiver<ServerMessage>,
        mpsc::Sender<ServerMessage>,
        CancellationToken,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let closed = CancellationToken::new();
        let mounts = Arc::new(
            Mounts::new(
                &defs(&["/mnt/m"]),
                MountLimits::default(),
                tx.downgrade(),
                closed.clone(),
            )
            .unwrap(),
        );
        let storage = RemoteStorage {
            mount: "/mnt/m".into(),
            remote: Arc::clone(&mounts.remote),
        };
        (storage, mounts, rx, tx, closed)
    }

    fn fs_request(msg: ServerMessage) -> FsRequest {
        match msg.message {
            Some(server_message::Message::FsRequest(req)) => req,
            other => panic!("not an fs_request: {other:?}"),
        }
    }

    fn file(size: usize) -> FsMetadata {
        FsMetadata {
            is_dir: false,
            size: size as u64,
            modified: Some(1.0),
        }
    }

    /// A fake client serving `files` (path → contents), counting the
    /// requests it gets
    fn serve(files: Vec<(&'static str, &'static [u8])>) -> (RemoteStorage, Arc<AtomicUsize>) {
        let (storage, mounts, mut rx, tx, _closed) = mount();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        tokio::spawn(async move {
            let _tx = tx;
            while let Some(msg) = rx.recv().await {
                let req = fs_request(msg);
                seen.fetch_add(1, Ordering::Relaxed);
                let file_at = files.iter().find(|(p, _)| *p == req.path);
                let is_dir = |p: &str| {
                    p.is_empty() || files.iter().any(|(f, _)| f.starts_with(&format!("{p}/")))
                };
                let result = match (req.op.unwrap(), file_at) {
                    (fs_request::Op::Stat(_), Some((_, c))) => {
                        fs_response::Result::Stat(file(c.len()))
                    }
                    (fs_request::Op::Stat(_), None) if is_dir(&req.path) => {
                        fs_response::Result::Stat(FsMetadata {
                            is_dir: true,
                            ..FsMetadata::default()
                        })
                    }
                    (fs_request::Op::List(_), None) if is_dir(&req.path) => {
                        let prefix = if req.path.is_empty() {
                            String::new()
                        } else {
                            format!("{}/", req.path)
                        };
                        let entries = files
                            .iter()
                            .filter_map(|(f, c)| {
                                let name = f.strip_prefix(&prefix)?;
                                (!name.contains('/')).then(|| FsEntry {
                                    name: name.into(),
                                    metadata: Some(file(c.len())),
                                    content: (c.len() <= 4).then(|| c.to_vec()),
                                })
                            })
                            .collect();
                        fs_response::Result::Listing(FsListing { entries })
                    }
                    (fs_request::Op::Read(read), Some((_, c))) => {
                        let from = usize::try_from(read.offset).unwrap().min(c.len());
                        let to = (from + usize::try_from(read.len).unwrap()).min(c.len());
                        fs_response::Result::Data(c[from..to].to_vec())
                    }
                    _ => fs_response::Result::Error(FsError {
                        code: FsErrorCode::NotFound.into(),
                        message: String::new(),
                    }),
                };
                mounts.reply(FsResponse {
                    op_id: req.op_id,
                    result: Some(result),
                });
            }
        });
        (storage, count)
    }

    static BIG: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| {
        (0..600_000u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect()
    });

    #[tokio::test]
    async fn reads_come_from_blocks_and_listings() {
        let (storage, count) = serve(vec![("a.txt", b"abc"), ("dir/big.bin", BIG.as_slice())]);
        let ops = || count.load(Ordering::Relaxed);

        // The root's listing brings a.txt's stat and contents
        let names: Vec<_> = storage
            .list("/mnt/m")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt"]);
        assert_eq!(storage.read("/mnt/m/a.txt").await.unwrap(), b"abc");
        assert!(!storage.exists("/mnt/m/missing").await.unwrap());
        assert_eq!(ops(), 1);

        // A read across blocks fetches each once, and the rest comes from them
        let at = BLOCK_SIZE - 10;
        let i = usize::try_from(at).unwrap();
        let data = storage
            .read_at("/mnt/m/dir/big.bin", at, 100)
            .await
            .unwrap();
        assert_eq!(data, &BIG[i..i + 100]);
        assert_eq!(ops(), 1 + 3, "a stat and two blocks");
        assert_eq!(storage.read("/mnt/m/dir/big.bin").await.unwrap(), *BIG);
        assert_eq!(ops(), 1 + 3 + 1, "and the third block");
        let past_end = storage.read_at("/mnt/m/dir/big.bin", 600_000, 10);
        assert_eq!(past_end.await.unwrap(), b"");

        assert!(matches!(
            storage.stat("/mnt/m/nope").await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            storage.read("/mnt/m/../x").await,
            Err(VfsError::InvalidPath(_))
        ));
        assert!(matches!(
            storage.write("/mnt/m/a.txt", b"x").await,
            Err(VfsError::PermissionDenied(_))
        ));
    }

    /// The guest polls an operation once and drops it, then runs it again:
    /// the rerun joins the first request rather than sending another
    #[tokio::test]
    async fn a_dropped_operation_is_joined_by_its_rerun() {
        let (storage, count) = serve(vec![("a.txt", b"hello")]);
        let mut first = storage.read("/mnt/m/a.txt");
        assert!(futures::poll!(first.as_mut()).is_pending());
        drop(first);
        assert_eq!(storage.read("/mnt/m/a.txt").await.unwrap(), b"hello");
        assert_eq!(count.load(Ordering::Relaxed), 2, "one stat, one read");
    }

    /// Once the client can't answer, waiting operations fail straight away,
    /// and so do new ones
    #[tokio::test]
    async fn operations_fail_once_the_client_is_gone() {
        let (storage, _mounts, mut rx, _tx, closed) = mount();
        let storage = Arc::new(storage);
        let waiting = Arc::clone(&storage);
        let stat = tokio::spawn(async move { waiting.stat("/mnt/m/a").await });
        rx.recv().await.unwrap();
        closed.cancel();
        assert!(matches!(stat.await.unwrap(), Err(VfsError::Io(_))));
        assert!(matches!(
            storage.stat("/mnt/m/b").await,
            Err(VfsError::Io(_))
        ));
    }

    /// Past the operation limit, new paths fail without a cache entry, so
    /// a guest probing paths can't grow the cache without bound
    #[tokio::test]
    async fn failures_past_the_limit_arent_cached() {
        let (storage, _count) = serve(vec![("a.txt", b"abc")]);
        storage
            .remote
            .ops
            .store(MountLimits::default().max_ops, Ordering::Relaxed);
        for i in 0..100 {
            assert!(matches!(
                storage.exists(&format!("/mnt/m/{i}")).await,
                Err(VfsError::Io(_))
            ));
            assert!(storage.list(&format!("/mnt/m/d{i}")).await.is_err());
        }
        let cache = lock(&storage.remote.cache);
        assert!(cache.stats.is_empty() && cache.lists.is_empty());
    }

    /// Stray answers are dropped without counting towards the transfer
    #[tokio::test]
    async fn stray_answers_dont_count() {
        let (_storage, mounts, _rx, _tx, _closed) = mount();
        mounts.reply(FsResponse {
            op_id: "nobody".into(),
            result: Some(fs_response::Result::Data(vec![0; 1000])),
        });
        assert_eq!(mounts.remote.received.load(Ordering::Relaxed), 0);
    }

    /// The mounts don't keep the response stream open: once the run's own
    /// senders are gone, their operations fail rather than send
    #[tokio::test]
    async fn the_mounts_dont_hold_the_stream_open() {
        let (storage, _mounts, mut rx, tx, _closed) = mount();
        drop(tx);
        assert!(rx.recv().await.is_none());
        assert!(matches!(
            storage.stat("/mnt/m/a").await,
            Err(VfsError::Io(_))
        ));
    }

    /// A client can't make the cache hold more than it asked for, nor answer
    /// with the wrong kind of result
    #[tokio::test]
    async fn bad_answers_fail() {
        let (storage, mounts, mut rx, _tx, _closed) = mount();
        let read = tokio::spawn(async move { storage.read("/mnt/m/f").await });
        let stat = fs_request(rx.recv().await.unwrap());
        mounts.reply(FsResponse {
            op_id: stat.op_id,
            result: Some(fs_response::Result::Stat(file(10))),
        });
        let block = fs_request(rx.recv().await.unwrap());
        mounts.reply(FsResponse {
            op_id: block.op_id,
            result: Some(fs_response::Result::Data(vec![0; 100])),
        });
        assert!(matches!(read.await.unwrap(), Err(VfsError::Io(_))));

        let (storage, mounts, mut rx, _tx, _closed) = mount();
        let list = tokio::spawn(async move { storage.list("/mnt/m").await });
        let req = fs_request(rx.recv().await.unwrap());
        mounts.reply(FsResponse {
            op_id: req.op_id,
            result: Some(fs_response::Result::Data(vec![])),
        });
        assert!(matches!(list.await.unwrap(), Err(VfsError::Io(_))));
    }

    /// A fetch left unfinished in the cache (its run timed out) holds the
    /// `Remote` holding the cache: closing the run's `Mounts` frees both
    #[tokio::test]
    async fn an_abandoned_fetch_doesnt_leak_the_run() {
        let (storage, mounts, _rx, _tx, _closed) = mount();
        let remote = Arc::downgrade(&mounts.remote);
        let mut stat = storage.stat("/mnt/m/a");
        assert!(futures::poll!(stat.as_mut()).is_pending());
        drop(stat);
        drop((storage, mounts));
        assert!(remote.upgrade().is_none());
    }

    #[test]
    fn times_past_systemtime_dont_panic() {
        let stat = Stat {
            is_dir: false,
            size: 0,
            modified: Some(1e19),
        };
        assert_eq!(metadata(&stat).modified, SystemTime::UNIX_EPOCH);
    }

    #[tokio::test]
    async fn the_cache_evicts_old_blocks() {
        const CACHE_BYTES: u64 = 4 * BLOCK_SIZE;
        let mut cache = Cache::new(CACHE_BYTES);
        let done = |b: &'static [u8]| fetched(Arc::new(b.to_vec()));
        for i in 0..(CACHE_BYTES / BLOCK_SIZE) {
            cache.reserve(BLOCK_SIZE).unwrap();
            cache.add_block(("/f".into(), i), BLOCK_SIZE, done(b"x"));
        }
        // An unfinished block stays; the oldest finished one goes
        let pending = futures::future::pending().boxed().shared();
        cache.blocks.insert(("/f".into(), 0), pending);
        cache.reserve(BLOCK_SIZE).unwrap();
        assert!(cache.blocks.contains_key(&("/f".into(), 0)));
        assert!(!cache.blocks.contains_key(&("/f".into(), 1)));
        assert!(cache.reserve(CACHE_BYTES + 1).is_err());
    }

    /// A writable mount's changes are relative to it, and a big file comes
    /// in chunks
    #[tokio::test]
    async fn changes_are_relative_and_chunked() {
        let (tx, mut rx) = mpsc::channel(16);
        let def = Mount {
            path: "/mnt/m".into(),
            writable: true,
        };
        let mounts = Arc::new(
            Mounts::new(
                &[def],
                MountLimits::default(),
                tx.downgrade(),
                CancellationToken::new(),
            )
            .unwrap(),
        );
        // A client with nothing in it
        let answers = Arc::clone(&mounts);
        tokio::spawn(async move {
            let _tx = tx;
            while let Some(msg) = rx.recv().await {
                answers.reply(FsResponse {
                    op_id: fs_request(msg).op_id,
                    result: Some(fs_response::Result::Error(FsError {
                        code: FsErrorCode::NotFound.into(),
                        message: String::new(),
                    })),
                });
            }
        });
        let overlay = mounts.mounts[0].1.as_ref().unwrap();
        let data: Vec<u8> = (0..CHUNK * 2 + 10).map(|i| (i % 251) as u8).collect();
        overlay.write("/mnt/m/f", &data).await.unwrap();
        overlay.write("/mnt/m/empty", b"").await.unwrap();
        let mut offsets = Vec::new();
        let mut joined = Vec::new();
        for c in mounts.changes().await.unwrap() {
            assert_eq!(c.mount, "/mnt/m");
            let Some(fs_change::Change::Write(w)) = c.change else {
                panic!("not a write: {c:?}");
            };
            offsets.push((c.path, w.offset));
            joined.extend(w.data);
        }
        let chunk = CHUNK as u64;
        let f = |offset| ("f".to_string(), offset);
        assert_eq!(offsets, [("empty".into(), 0), f(0), f(chunk), f(2 * chunk)]);
        assert_eq!(joined, data);
    }
}
