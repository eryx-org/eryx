//! Hybrid VFS that serves each preopen from either VFS storage or the real filesystem.
//!
//! This module provides a filesystem implementation that combines:
//! - VFS storage for sandboxed preopens (e.g., `/eryx`)
//! - Real filesystem passthrough for system preopens (e.g., `/python-stdlib`)
//!
//! This allows Python to access its stdlib while still providing an isolated
//! writable filesystem area for user code.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use cap_primitives::fs as capfs;

use crate::fork_local::ForkLocal;
use crate::perms::{DirPerms, FilePerms};
use wasmtime::component::ResourceTable;

use crate::storage::VfsStorage;
use crate::wasi_impl::VfsDescriptor;

/// A capability-restricted directory handle.
///
/// Wraps a sandboxed directory file descriptor and enforces `file_map`
/// restrictions on every filesystem operation. The inner fd is private, so
/// there is no way to bypass the filter — access control is enforced by the
/// type system, not by convention.
///
/// When `file_map` is `None`, all child paths are allowed (normal directory
/// mount). When `file_map` is `Some`, only mapped guest filenames can be
/// accessed, and they are transparently translated to host filenames. Such a
/// handle exposes files only: the mapped names can't be opened as or turned
/// into directories, renamed, or followed if they are symlinks, so the rest
/// of the host directory stays out of reach.
#[derive(Clone)]
pub struct RestrictedDir {
    inner: Arc<std::fs::File>,
    file_map: Option<HashMap<String, String>>,
}

impl std::fmt::Debug for RestrictedDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestrictedDir")
            .field("restricted", &self.file_map.is_some())
            .finish_non_exhaustive()
    }
}

impl RestrictedDir {
    /// Create an unrestricted directory handle (all child paths allowed).
    pub fn new(dir: std::fs::File) -> Self {
        Self {
            inner: Arc::new(dir),
            file_map: None,
        }
    }

    /// Create a restricted directory handle with a guest-to-host filename map.
    ///
    /// Only filenames present as keys in the map will be accessible.
    /// Guest filenames are transparently translated to host filenames.
    pub fn with_file_map(dir: std::fs::File, file_map: HashMap<String, String>) -> Self {
        Self {
            inner: Arc::new(dir),
            file_map: Some(file_map),
        }
    }

    /// Open an ambient directory as an unrestricted handle.
    pub fn open_ambient(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = capfs::open_ambient_dir(path.as_ref(), ambient_authority::ambient_authority())?;
        Ok(Self::new(dir))
    }

    /// Refuse an operation that a file-only (mapped) handle doesn't allow.
    fn check_unrestricted(&self, guest_rel_path: &str) -> Result<(), std::io::Error> {
        if self.file_map.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("not allowed on a single-file mount: {guest_rel_path}"),
            ));
        }
        Ok(())
    }

    /// The host directory handle, for counting open host descriptors.
    pub(crate) fn handle(&self) -> &Arc<std::fs::File> {
        &self.inner
    }

    /// Check whether a guest-relative path is allowed.
    fn check_allowed(&self, guest_rel_path: &str) -> Result<(), std::io::Error> {
        match &self.file_map {
            None => Ok(()),
            Some(map) => {
                let normalized = guest_rel_path.strip_prefix("./").unwrap_or(guest_rel_path);
                if map.contains_key(normalized) {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("access denied: {guest_rel_path}"),
                    ))
                }
            }
        }
    }

    /// Translate a guest-relative path to a host-relative path.
    fn translate<'a>(&'a self, guest_rel_path: &'a str) -> &'a str {
        match &self.file_map {
            None => guest_rel_path,
            Some(map) => {
                let normalized = guest_rel_path.strip_prefix("./").unwrap_or(guest_rel_path);
                map.get(normalized)
                    .map(String::as_str)
                    .unwrap_or(guest_rel_path)
            }
        }
    }

    // ========================================================================
    // Filesystem operations — each checks + translates before delegating
    // ========================================================================

    /// Open a file within this directory.
    ///
    /// On a single-file mount the mapped name is opened without following a
    /// symlink, so replacing it with a link can't redirect the open.
    pub fn open_with(
        &self,
        guest_path: &str,
        opts: &capfs::OpenOptions,
    ) -> std::io::Result<std::fs::File> {
        self.check_allowed(guest_path)?;
        if self.file_map.is_some() {
            let mut opts = opts.clone();
            opts._cap_fs_ext_follow(capfs::FollowSymlinks::No);
            return capfs::open(&self.inner, self.translate(guest_path).as_ref(), &opts);
        }
        capfs::open(&self.inner, self.translate(guest_path).as_ref(), opts)
    }

    /// Open a subdirectory. Not allowed on a single-file mount.
    pub fn open_dir(&self, guest_path: &str) -> std::io::Result<RestrictedDir> {
        self.check_allowed(guest_path)?;
        self.check_unrestricted(guest_path)?;
        let sub = capfs::open_dir(&self.inner, self.translate(guest_path).as_ref())?;
        Ok(RestrictedDir {
            inner: Arc::new(sub),
            file_map: None,
        })
    }

    /// Create a subdirectory. Not allowed on a single-file mount.
    pub fn create_dir(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        self.check_unrestricted(guest_path)?;
        capfs::create_dir(
            &self.inner,
            self.translate(guest_path).as_ref(),
            &capfs::DirOptions::new(),
        )
    }

    /// Get metadata for a child path.
    pub fn metadata(&self, guest_path: &str) -> std::io::Result<capfs::Metadata> {
        self.check_allowed(guest_path)?;
        capfs::stat(
            &self.inner,
            self.translate(guest_path).as_ref(),
            capfs::FollowSymlinks::Yes,
        )
    }

    /// Set the access and/or modification time of a child path.
    pub fn set_times(
        &self,
        guest_path: &str,
        atime: Option<std::time::SystemTime>,
        mtime: Option<std::time::SystemTime>,
        follow: bool,
    ) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        let spec = |t: std::time::SystemTime| {
            capfs::SystemTimeSpec::Absolute(cap_primitives::time::SystemTime::from_std(t))
        };
        let path = self.translate(guest_path).as_ref();
        if follow {
            capfs::set_times(&self.inner, path, atime.map(spec), mtime.map(spec))
        } else {
            capfs::set_times_nofollow(&self.inner, path, atime.map(spec), mtime.map(spec))
        }
    }

    /// Set the times of the directory itself (no child path, no filter).
    pub fn set_dir_times(&self, times: std::fs::FileTimes) -> std::io::Result<()> {
        self.inner.set_times(times)
    }

    /// Get metadata for the directory itself (no child path, no filter).
    pub fn dir_metadata(&self) -> std::io::Result<capfs::Metadata> {
        capfs::Metadata::from_file(&self.inner)
    }

    /// Read a symbolic link.
    pub fn read_link(&self, guest_path: &str) -> std::io::Result<std::path::PathBuf> {
        self.check_allowed(guest_path)?;
        capfs::read_link(&self.inner, self.translate(guest_path).as_ref())
    }

    /// Remove a subdirectory.
    pub fn remove_dir(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        capfs::remove_dir(&self.inner, self.translate(guest_path).as_ref())
    }

    /// Remove a file.
    pub fn remove_file(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        capfs::remove_file(&self.inner, self.translate(guest_path).as_ref())
    }

    /// Rename a file or directory. Both source and destination are checked,
    /// and neither may be a single-file mount.
    pub fn rename(
        &self,
        old_guest: &str,
        dest: &RestrictedDir,
        new_guest: &str,
    ) -> std::io::Result<()> {
        self.check_allowed(old_guest)?;
        dest.check_allowed(new_guest)?;
        self.check_unrestricted(old_guest)?;
        dest.check_unrestricted(new_guest)?;
        capfs::rename(
            &self.inner,
            self.translate(old_guest).as_ref(),
            &dest.inner,
            dest.translate(new_guest).as_ref(),
        )
    }

    /// List directory entries, filtered and translated through the file map.
    ///
    /// If restricted, only mapped files are returned with guest-visible names.
    /// If unrestricted, all entries are returned as-is.
    pub fn entries(&self) -> std::io::Result<Vec<capfs::DirEntry>> {
        match &self.file_map {
            None => capfs::read_dir(&self.inner, ".".as_ref())?.collect::<Result<Vec<_>, _>>(),
            Some(map) => {
                let reverse: HashMap<&str, &str> = map
                    .iter()
                    .map(|(guest, host)| (host.as_str(), guest.as_str()))
                    .collect();
                let mut result = Vec::new();
                for entry in capfs::read_dir(&self.inner, ".".as_ref())? {
                    let entry = entry?;
                    let host_name = entry.file_name().to_string_lossy().into_owned();
                    if reverse.contains_key(host_name.as_str()) {
                        result.push(entry);
                    }
                }
                Ok(result)
            }
        }
    }

    /// Get the guest-visible name for a directory entry.
    ///
    /// If restricted, translates the host filename back to the guest filename.
    /// If unrestricted, returns the entry's filename as-is.
    pub fn guest_name(&self, entry: &capfs::DirEntry) -> String {
        let host_name = entry.file_name().to_string_lossy().into_owned();
        match &self.file_map {
            None => host_name,
            Some(map) => {
                for (guest, host) in map {
                    if host == &host_name {
                        return guest.clone();
                    }
                }
                host_name
            }
        }
    }
}

/// A real filesystem directory handle.
///
/// This wraps a [`RestrictedDir`] with permissions and configuration.
/// The underlying directory fd is not directly accessible — all
/// filesystem operations go through `RestrictedDir`'s access control.
#[derive(Clone)]
pub struct RealDir {
    /// The restricted directory handle (enforces file_map at type level).
    pub dir: RestrictedDir,
    /// Directory permissions.
    pub dir_perms: DirPerms,
    /// Default file permissions for files in this directory.
    pub file_perms: FilePerms,
    /// Largest size, in bytes, a guest may grow a file beneath this directory
    /// to, by writing or truncating. `None` (the default) leaves writes
    /// unmetered: a writable host directory can then fill the host disk.
    pub max_file_size: Option<u64>,
}

impl std::fmt::Debug for RealDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealDir")
            .field("dir", &self.dir)
            .field("dir_perms", &self.dir_perms)
            .field("file_perms", &self.file_perms)
            .field("max_file_size", &self.max_file_size)
            .finish_non_exhaustive()
    }
}

impl RealDir {
    /// Create a new RealDir from a directory file descriptor.
    pub fn new(dir: std::fs::File, dir_perms: DirPerms, file_perms: FilePerms) -> Self {
        Self {
            dir: RestrictedDir::new(dir),
            dir_perms,
            file_perms,
            max_file_size: None,
        }
    }

    /// Limit how large a guest may make any file beneath this directory.
    ///
    /// Writes or truncations past `bytes` fail with `EFBIG`. Short of this,
    /// guest writes to a writable host directory are not metered.
    #[must_use]
    pub fn with_max_file_size(mut self, bytes: u64) -> Self {
        self.max_file_size = Some(bytes);
        self
    }

    /// Open a directory from a path.
    pub fn open_ambient(
        path: impl AsRef<Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> std::io::Result<Self> {
        let dir = capfs::open_ambient_dir(path.as_ref(), ambient_authority::ambient_authority())?;
        Ok(Self::new(dir, dir_perms, file_perms))
    }
}

/// A real filesystem file handle.
pub struct RealFile {
    /// The underlying file.
    pub file: Arc<std::fs::File>,
    /// File permissions.
    pub perms: FilePerms,
    /// Whether the file is open for reading.
    pub readable: bool,
    /// Whether the file is open for writing.
    pub writable: bool,
    /// Largest size a guest may grow the file to; see
    /// [`RealDir::max_file_size`].
    pub max_size: Option<u64>,
}

impl RealFile {
    /// Fail if writing up to `end` would grow the file past its size limit.
    pub(crate) fn check_size(&self, end: u64) -> std::io::Result<()> {
        check_file_size(self.max_size, end)
    }
}

/// Fail with `EFBIG` if `end` is past `max_size`.
pub(crate) fn check_file_size(max_size: Option<u64>, end: u64) -> std::io::Result<()> {
    match max_size {
        Some(max) if end > max => Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file would grow to {end} bytes, over the {max} byte limit"),
        )),
        _ => Ok(()),
    }
}

impl std::fmt::Debug for RealFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealFile")
            .field("perms", &self.perms)
            .field("readable", &self.readable)
            .field("writable", &self.writable)
            .field("max_size", &self.max_size)
            .finish_non_exhaustive()
    }
}

/// A descriptor that can be either VFS-managed or real filesystem.
pub enum HybridDescriptor {
    /// A descriptor for VFS-managed paths (e.g., /data/*)
    Vfs(VfsDescriptor),
    /// A real filesystem directory.
    RealDir {
        /// The directory handle.
        dir: RealDir,
        /// The guest path this descriptor was opened from.
        guest_path: String,
    },
    /// A real filesystem file.
    RealFile {
        /// The file handle.
        file: RealFile,
        /// The guest path this descriptor was opened from.
        guest_path: String,
    },
}

impl std::fmt::Debug for HybridDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HybridDescriptor::Vfs(d) => f.debug_tuple("Vfs").field(d).finish(),
            HybridDescriptor::RealDir { guest_path, .. } => f
                .debug_struct("RealDir")
                .field("guest_path", guest_path)
                .finish_non_exhaustive(),
            HybridDescriptor::RealFile { guest_path, .. } => f
                .debug_struct("RealFile")
                .field("guest_path", guest_path)
                .finish_non_exhaustive(),
        }
    }
}

/// Configuration for a preopen directory.
pub enum HybridPreopen {
    /// A VFS-managed preopen (virtual storage).
    Vfs {
        /// Guest-visible path (e.g., "/data")
        guest_path: String,
        /// Directory permissions
        dir_perms: DirPerms,
        /// File permissions for files in this directory
        file_perms: FilePerms,
    },
    /// A real filesystem preopen.
    Real {
        /// Guest-visible path (e.g., "/python-stdlib")
        guest_path: String,
        /// The directory handle.
        dir: RealDir,
    },
}

impl std::fmt::Debug for HybridPreopen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HybridPreopen::Vfs {
                guest_path,
                dir_perms,
                file_perms,
            } => f
                .debug_struct("Vfs")
                .field("guest_path", guest_path)
                .field("dir_perms", dir_perms)
                .field("file_perms", file_perms)
                .finish(),
            HybridPreopen::Real { guest_path, dir } => f
                .debug_struct("Real")
                .field("guest_path", guest_path)
                .field("dir", dir)
                .finish(),
        }
    }
}

impl HybridPreopen {
    /// Get the guest path for this preopen.
    pub fn guest_path(&self) -> &str {
        match self {
            HybridPreopen::Vfs { guest_path, .. } => guest_path,
            HybridPreopen::Real { guest_path, .. } => guest_path,
        }
    }
}

/// Context for hybrid VFS operations.
///
/// This combines VFS storage with real filesystem passthrough. Each preopen
/// is either VFS-backed or a real directory, and every descriptor opened
/// beneath it stays on the same side.
pub struct HybridVfsCtx<S: VfsStorage + Clone> {
    /// The VFS storage backend for virtual paths.
    pub storage: S,
    /// Preopened directories (both VFS and real).
    pub preopens: Vec<HybridPreopen>,
    /// Host files and directories the guest has opened. A handle stays open
    /// while any descriptor or stream holds it, so dead entries are pruned
    /// when the count is checked.
    open_host_files: Vec<Weak<std::fs::File>>,
    /// Limit on `open_host_files`.
    max_open_host_files: usize,
    /// Budget shared with every other context in the process.
    process_budget: &'static HostFileBudget,
}

/// Default limit on host files and directories one guest can hold open at
/// once beneath real-directory preopens.
///
/// Every open is a host file descriptor, and the process's descriptor limit is
/// shared with everything else running in it.
pub const DEFAULT_MAX_OPEN_HOST_FILES: usize = 256;

/// Host files and directories open across every [`HybridVfsCtx`] in the
/// process, so many concurrent guests can't exhaust the process's descriptor
/// limit between them.
static PROCESS_HOST_FILES: HostFileBudget = HostFileBudget::new(UNSET);

/// `max` value meaning "derive the default on first use".
const UNSET: usize = usize::MAX;

/// Set how many host files and directories all guests in this process may
/// hold open at once, summed over every [`HybridVfsCtx`]. Opens past the limit
/// fail with the same quota error as the per-context
/// [`with_max_open_host_files`](HybridVfsCtx::with_max_open_host_files) limit.
///
/// The default is half the process's soft `RLIMIT_NOFILE`, read once, the
/// first time a guest opens a host file, and never below
/// [`DEFAULT_MAX_OPEN_HOST_FILES`] (so a low limit such as macOS's usual 256
/// doesn't undercut one sandbox's own limit). On non-Unix platforms, or with
/// no limit set, it is 4096. Raise `RLIMIT_NOFILE` before the first open for a
/// larger default. Passing `usize::MAX` restores the default.
pub fn set_process_max_open_host_files(max: usize) {
    PROCESS_HOST_FILES.max.store(max, Ordering::Relaxed);
}

/// The current process-wide limit; see [`set_process_max_open_host_files`].
pub fn process_max_open_host_files() -> usize {
    PROCESS_HOST_FILES.max()
}

/// Fallback process-wide limit when no descriptor limit can be read.
const FALLBACK_PROCESS_MAX_OPEN_HOST_FILES: usize = 4096;

fn default_process_max_open_host_files() -> usize {
    #[cfg(unix)]
    if let Some(soft) = rustix::process::getrlimit(rustix::process::Resource::Nofile).current {
        return usize::try_from(soft / 2)
            .unwrap_or(usize::MAX)
            .max(DEFAULT_MAX_OPEN_HOST_FILES);
    }
    FALLBACK_PROCESS_MAX_OPEN_HOST_FILES
}

/// A limit on host handles shared by several contexts.
pub(crate) struct HostFileBudget {
    /// Per process, so a forked child neither counts the parent's handles nor
    /// waits on a lock a parent thread held at the fork.
    open: ForkLocal<Mutex<Vec<Weak<std::fs::File>>>>,
    max: AtomicUsize,
}

impl HostFileBudget {
    pub(crate) const fn new(max: usize) -> Self {
        Self {
            open: ForkLocal::new(|| Mutex::new(Vec::new())),
            max: AtomicUsize::new(max),
        }
    }

    fn max(&self) -> usize {
        match self.max.load(Ordering::Relaxed) {
            UNSET => {
                let max = default_process_max_open_host_files();
                // A concurrent explicit set wins over the derived default.
                let _ = self
                    .max
                    .compare_exchange(UNSET, max, Ordering::Relaxed, Ordering::Relaxed);
                self.max.load(Ordering::Relaxed)
            }
            max => max,
        }
    }

    /// Count `file` against the budget, or fail if it is used up.
    fn admit(&self, file: &Arc<std::fs::File>) -> crate::VfsResult<()> {
        let max = self.max();
        // The list holds no invariant a panic could break, so a poisoned lock
        // is still usable.
        let mut open = self.open.get().lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|f| f.strong_count() > 0);
        if open.len() >= max {
            return Err(crate::VfsError::TooManyOpenFiles(format!(
                "process-wide limit of {max} open host files reached"
            )));
        }
        open.push(Arc::downgrade(file));
        Ok(())
    }
}

impl<S: VfsStorage + Clone> std::fmt::Debug for HybridVfsCtx<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridVfsCtx")
            .field("preopens", &self.preopens.len())
            .finish_non_exhaustive()
    }
}

impl<S: VfsStorage + Clone> HybridVfsCtx<S> {
    /// Create a new hybrid VFS context with the given storage.
    ///
    /// By default, no paths are configured. Use `add_vfs_preopen` and
    /// `add_real_preopen` to configure paths.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            preopens: Vec::new(),
            open_host_files: Vec::new(),
            max_open_host_files: DEFAULT_MAX_OPEN_HOST_FILES,
            process_budget: &PROCESS_HOST_FILES,
        }
    }

    /// Set how many host files and directories the guest may hold open at
    /// once (default [`DEFAULT_MAX_OPEN_HOST_FILES`]). Opens past the limit
    /// fail until the guest closes some.
    #[must_use]
    pub fn with_max_open_host_files(mut self, max: usize) -> Self {
        self.max_open_host_files = max;
        self
    }

    /// Fail if the guest already holds the maximum number of host handles.
    pub(crate) fn check_open_host_files(&mut self) -> crate::VfsResult<()> {
        self.open_host_files.retain(|f| f.strong_count() > 0);
        if self.open_host_files.len() >= self.max_open_host_files {
            return Err(crate::VfsError::TooManyOpenFiles(format!(
                "limit of {} open host files reached",
                self.max_open_host_files
            )));
        }
        Ok(())
    }

    /// Count a newly opened host handle against this context's limit and the
    /// process-wide one. On error the caller drops `file`, closing it.
    pub(crate) fn track_open_host_file(
        &mut self,
        file: &Arc<std::fs::File>,
    ) -> crate::VfsResult<()> {
        self.process_budget.admit(file)?;
        self.open_host_files.push(Arc::downgrade(file));
        Ok(())
    }

    /// Share `budget` instead of the process-wide one, so tests can use a
    /// small limit without affecting each other.
    #[cfg(test)]
    pub(crate) fn with_process_budget(mut self, budget: &'static HostFileBudget) -> Self {
        self.process_budget = budget;
        self
    }

    /// Add a VFS-managed preopen directory.
    ///
    /// Descriptors opened under this preopen are handled by VFS storage.
    /// The directory will be created in storage if it doesn't exist.
    pub fn add_vfs_preopen(
        &mut self,
        guest_path: impl Into<String>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) {
        let guest_path = guest_path.into();

        // Ensure the directory exists in storage.
        // We use the sync method since this is called during setup before
        // the async runtime might be fully available.
        if let Err(e) = self.storage.mkdir_sync(&guest_path) {
            tracing::warn!(
                "Failed to create VFS preopen directory {}: {}",
                guest_path,
                e
            );
        }

        self.preopens.push(HybridPreopen::Vfs {
            guest_path,
            dir_perms,
            file_perms,
        });
    }

    /// Add a real filesystem preopen directory.
    ///
    /// Paths under `guest_path` will be passed through to the real filesystem.
    pub fn add_real_preopen(&mut self, guest_path: impl Into<String>, dir: RealDir) {
        self.preopens.push(HybridPreopen::Real {
            guest_path: guest_path.into(),
            dir,
        });
    }

    /// Add a real filesystem preopen from a host path.
    ///
    /// Opens the directory at `host_path` and maps it to `guest_path`.
    pub fn add_real_preopen_path(
        &mut self,
        guest_path: impl Into<String>,
        host_path: impl AsRef<Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> std::io::Result<()> {
        let dir = RealDir::open_ambient(host_path, dir_perms, file_perms)?;
        self.add_real_preopen(guest_path, dir);
        Ok(())
    }

    /// Add a real filesystem preopen for a single host file.
    ///
    /// This mounts the file's parent directory at the guest file's parent path,
    /// restricted so only the specified filename is accessible.
    ///
    /// For example, mounting host `/home/ben/fetch.py` at guest `/mnt/fetch.py`
    /// opens `/home/ben/` at `/mnt/` but only allows access to `fetch.py`.
    /// The guest can read, write, replace or remove that file (as the
    /// permissions allow) but can't list, open or create anything else in the
    /// host directory. A `host_path` that is a symlink is resolved here, once.
    /// `max_file_size` works as in [`RealDir::with_max_file_size`].
    pub fn add_real_file_preopen_path(
        &mut self,
        guest_path: impl Into<String>,
        host_path: impl AsRef<Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
        max_file_size: Option<u64>,
    ) -> std::io::Result<()> {
        // Resolve symlinks now: the mapped name is later opened without
        // following one.
        let host_path = &std::fs::canonicalize(host_path.as_ref())?;
        let guest_path = guest_path.into();

        let host_parent = host_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("file has no parent directory: {}", host_path.display()),
            )
        })?;

        let file_name = host_path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("file has no name: {}", host_path.display()),
                )
            })?
            .to_string_lossy()
            .into_owned();

        let guest_parent = guest_path
            .rsplit_once('/')
            .map(|(parent, _)| {
                if parent.is_empty() {
                    "/".to_string()
                } else {
                    parent.to_string()
                }
            })
            .unwrap_or_else(|| "/".to_string());

        let guest_file_name = guest_path
            .rsplit_once('/')
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| guest_path.clone());

        let raw_dir = capfs::open_ambient_dir(host_parent, ambient_authority::ambient_authority())?;
        let restricted =
            RestrictedDir::with_file_map(raw_dir, HashMap::from([(guest_file_name, file_name)]));
        let dir = RealDir {
            dir: restricted,
            dir_perms,
            file_perms,
            max_file_size,
        };
        self.add_real_preopen(guest_parent, dir);
        Ok(())
    }
}

/// A view into the hybrid VFS state for WASI trait implementations.
pub struct HybridVfsState<'a, S: VfsStorage + Clone> {
    /// The hybrid VFS context.
    pub ctx: &'a mut HybridVfsCtx<S>,
    /// The resource table for managing descriptors.
    pub table: &'a mut ResourceTable,
}

impl<S: VfsStorage + Clone> std::fmt::Debug for HybridVfsState<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridVfsState")
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

impl<'a, S: VfsStorage + Clone> HybridVfsState<'a, S> {
    /// Create a new hybrid VFS state.
    pub fn new(ctx: &'a mut HybridVfsCtx<S>, table: &'a mut ResourceTable) -> Self {
        Self { ctx, table }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn single_file_mount_exposes_only_the_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("mapped"), b"mapped").unwrap();
        std::fs::write(temp.path().join("sibling"), b"sibling").unwrap();
        let handle =
            capfs::open_ambient_dir(temp.path(), ambient_authority::ambient_authority()).unwrap();
        let dir =
            RestrictedDir::with_file_map(handle, HashMap::from([("f".into(), "mapped".into())]));
        let denied = |r: std::io::Result<()>| {
            assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        };

        denied(dir.open_dir("f").map(drop));
        denied(dir.create_dir("f"));
        denied(dir.rename("f", &dir, "f"));
        denied(
            dir.open_with("sibling", &capfs::OpenOptions::new())
                .map(drop),
        );

        // A symlink planted at the mapped name is not followed.
        std::fs::remove_file(temp.path().join("mapped")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("sibling", temp.path().join("mapped")).unwrap();
            let mut opts = capfs::OpenOptions::new();
            opts.read(true);
            assert!(dir.open_with("f", &opts).is_err());
        }
    }

    #[test]
    fn test_hybrid_preopen_guest_path() {
        let preopen = HybridPreopen::Vfs {
            guest_path: "/data".to_string(),
            dir_perms: DirPerms::all(),
            file_perms: FilePerms::all(),
        };
        assert_eq!(preopen.guest_path(), "/data");
    }
}
