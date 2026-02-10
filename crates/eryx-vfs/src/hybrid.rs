//! Hybrid VFS that routes paths to either VFS storage or real filesystem.
//!
//! This module provides a filesystem implementation that combines:
//! - VFS storage for sandboxed paths (e.g., `/data/*`)
//! - Real filesystem passthrough for system paths (e.g., `/python-stdlib/*`)
//!
//! This allows Python to access its stdlib while still providing an isolated
//! writable filesystem area for user code.

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(windows)]
use cap_fs_ext::DirExt as _;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{DirPerms, FilePerms};

use crate::storage::VfsStorage;
use crate::wasi_impl::VfsDescriptor;

/// A capability-restricted directory handle.
///
/// Wraps `cap_std::fs::Dir` and enforces `file_map` restrictions on every
/// filesystem operation. The inner `Dir` is private, so there is no way to
/// bypass the filter — access control is enforced by the type system, not
/// by convention.
///
/// When `file_map` is `None`, all child paths are allowed (normal directory
/// mount). When `file_map` is `Some`, only mapped guest filenames can be
/// accessed, and they are transparently translated to host filenames.
#[derive(Clone)]
pub struct RestrictedDir {
    inner: Arc<cap_std::fs::Dir>,
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
    pub fn new(dir: cap_std::fs::Dir) -> Self {
        Self {
            inner: Arc::new(dir),
            file_map: None,
        }
    }

    /// Create a restricted directory handle with a guest-to-host filename map.
    ///
    /// Only filenames present as keys in the map will be accessible.
    /// Guest filenames are transparently translated to host filenames.
    pub fn with_file_map(dir: cap_std::fs::Dir, file_map: HashMap<String, String>) -> Self {
        Self {
            inner: Arc::new(dir),
            file_map: Some(file_map),
        }
    }

    /// Open an ambient directory as an unrestricted handle.
    pub fn open_ambient(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        Ok(Self::new(dir))
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
    pub fn open_with(
        &self,
        guest_path: &str,
        opts: &cap_std::fs::OpenOptions,
    ) -> std::io::Result<cap_std::fs::File> {
        self.check_allowed(guest_path)?;
        self.inner.open_with(self.translate(guest_path), opts)
    }

    /// Open a subdirectory. The returned `RestrictedDir` is unrestricted
    /// (subdirectories don't inherit single-file restrictions).
    pub fn open_dir(&self, guest_path: &str) -> std::io::Result<RestrictedDir> {
        self.check_allowed(guest_path)?;
        let sub = self.inner.open_dir(self.translate(guest_path))?;
        Ok(RestrictedDir {
            inner: Arc::new(sub),
            file_map: None,
        })
    }

    /// Create a subdirectory.
    pub fn create_dir(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        self.inner.create_dir(self.translate(guest_path))
    }

    /// Get metadata for a child path.
    pub fn metadata(&self, guest_path: &str) -> std::io::Result<cap_std::fs::Metadata> {
        self.check_allowed(guest_path)?;
        self.inner.metadata(self.translate(guest_path))
    }

    /// Get metadata for the directory itself (no child path, no filter).
    pub fn dir_metadata(&self) -> std::io::Result<cap_std::fs::Metadata> {
        self.inner.dir_metadata()
    }

    /// Read a symbolic link.
    pub fn read_link(&self, guest_path: &str) -> std::io::Result<std::path::PathBuf> {
        self.check_allowed(guest_path)?;
        self.inner.read_link(self.translate(guest_path))
    }

    /// Remove a subdirectory.
    pub fn remove_dir(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        self.inner.remove_dir(self.translate(guest_path))
    }

    /// Remove a file.
    pub fn remove_file(&self, guest_path: &str) -> std::io::Result<()> {
        self.check_allowed(guest_path)?;
        self.inner.remove_file(self.translate(guest_path))
    }

    /// Rename a file or directory. Both source and destination are checked.
    pub fn rename(
        &self,
        old_guest: &str,
        dest: &RestrictedDir,
        new_guest: &str,
    ) -> std::io::Result<()> {
        self.check_allowed(old_guest)?;
        dest.check_allowed(new_guest)?;
        self.inner.rename(
            self.translate(old_guest),
            &dest.inner,
            dest.translate(new_guest),
        )
    }

    /// Create a symbolic link.
    pub fn symlink(&self, src_path: &str, dest_guest: &str) -> std::io::Result<()> {
        self.check_allowed(dest_guest)?;
        self.inner.symlink(src_path, self.translate(dest_guest))
    }

    /// List directory entries, filtered and translated through the file map.
    ///
    /// If restricted, only mapped files are returned with guest-visible names.
    /// If unrestricted, all entries are returned as-is.
    pub fn entries(&self) -> std::io::Result<Vec<cap_std::fs::DirEntry>> {
        match &self.file_map {
            None => self.inner.entries()?.collect::<Result<Vec<_>, _>>(),
            Some(map) => {
                // Build reverse map: host_name → guest_name
                let reverse: HashMap<&str, &str> = map
                    .iter()
                    .map(|(guest, host)| (host.as_str(), guest.as_str()))
                    .collect();
                let mut result = Vec::new();
                for entry in self.inner.entries()? {
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
    pub fn guest_name(&self, entry: &cap_std::fs::DirEntry) -> String {
        let host_name = entry.file_name().to_string_lossy().into_owned();
        match &self.file_map {
            None => host_name,
            Some(map) => {
                // Reverse lookup: find the guest name for this host name
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
/// The underlying `cap_std::fs::Dir` is not directly accessible — all
/// filesystem operations go through `RestrictedDir`'s access control.
#[derive(Clone)]
pub struct RealDir {
    /// The restricted directory handle (enforces file_map at type level).
    pub dir: RestrictedDir,
    /// Directory permissions.
    pub dir_perms: DirPerms,
    /// Default file permissions for files in this directory.
    pub file_perms: FilePerms,
    /// Whether to allow blocking the current thread.
    pub allow_blocking: bool,
}

impl std::fmt::Debug for RealDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealDir")
            .field("dir", &self.dir)
            .field("dir_perms", &self.dir_perms)
            .field("file_perms", &self.file_perms)
            .finish_non_exhaustive()
    }
}

impl RealDir {
    /// Create a new RealDir from a cap-std Dir.
    pub fn new(dir: cap_std::fs::Dir, dir_perms: DirPerms, file_perms: FilePerms) -> Self {
        Self {
            dir: RestrictedDir::new(dir),
            dir_perms,
            file_perms,
            allow_blocking: false,
        }
    }

    /// Open a directory from a path.
    pub fn open_ambient(
        path: impl AsRef<std::path::Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> std::io::Result<Self> {
        let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        Ok(Self::new(dir, dir_perms, file_perms))
    }
}

/// A real filesystem file handle.
pub struct RealFile {
    /// The underlying cap-std file.
    pub file: Arc<cap_std::fs::File>,
    /// File permissions.
    pub perms: FilePerms,
    /// Whether the file is open for reading.
    pub readable: bool,
    /// Whether the file is open for writing.
    pub writable: bool,
    /// Whether to allow blocking the current thread.
    pub allow_blocking: bool,
}

impl std::fmt::Debug for RealFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealFile")
            .field("perms", &self.perms)
            .field("readable", &self.readable)
            .field("writable", &self.writable)
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

impl HybridDescriptor {
    /// Get the path this descriptor refers to.
    pub fn path(&self) -> &str {
        match self {
            HybridDescriptor::Vfs(d) => &d.path,
            HybridDescriptor::RealDir { guest_path, .. } => guest_path,
            HybridDescriptor::RealFile { guest_path, .. } => guest_path,
        }
    }

    /// Check if this is a directory.
    pub fn is_dir(&self) -> bool {
        match self {
            HybridDescriptor::Vfs(d) => d.is_dir,
            HybridDescriptor::RealDir { .. } => true,
            HybridDescriptor::RealFile { .. } => false,
        }
    }

    /// Get as VFS descriptor, if it is one.
    pub fn as_vfs(&self) -> Option<&VfsDescriptor> {
        match self {
            HybridDescriptor::Vfs(d) => Some(d),
            _ => None,
        }
    }

    /// Get as real directory, if it is one.
    pub fn as_real_dir(&self) -> Option<&RealDir> {
        match self {
            HybridDescriptor::RealDir { dir, .. } => Some(dir),
            _ => None,
        }
    }

    /// Get as real file, if it is one.
    pub fn as_real_file(&self) -> Option<&RealFile> {
        match self {
            HybridDescriptor::RealFile { file, .. } => Some(file),
            _ => None,
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
/// This combines VFS storage with real filesystem passthrough, routing
/// operations based on path prefixes.
pub struct HybridVfsCtx<S: VfsStorage> {
    /// The VFS storage backend for virtual paths.
    pub storage: Arc<S>,
    /// Path prefixes that should be handled by VFS (e.g., ["/data"]).
    pub vfs_prefixes: Vec<String>,
    /// Preopened directories (both VFS and real).
    pub preopens: Vec<HybridPreopen>,
    /// Whether to allow blocking the current thread for filesystem operations.
    pub allow_blocking_current_thread: bool,
}

impl<S: VfsStorage> std::fmt::Debug for HybridVfsCtx<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridVfsCtx")
            .field("vfs_prefixes", &self.vfs_prefixes)
            .field("preopens", &self.preopens.len())
            .field(
                "allow_blocking_current_thread",
                &self.allow_blocking_current_thread,
            )
            .finish_non_exhaustive()
    }
}

impl<S: VfsStorage> HybridVfsCtx<S> {
    /// Create a new hybrid VFS context with the given storage.
    ///
    /// By default, no paths are configured. Use `add_vfs_preopen` and
    /// `add_real_preopen` to configure paths.
    pub fn new(storage: Arc<S>) -> Self {
        Self {
            storage,
            vfs_prefixes: Vec::new(),
            preopens: Vec::new(),
            allow_blocking_current_thread: false,
        }
    }

    /// Add a VFS-managed preopen directory.
    ///
    /// Paths under `guest_path` will be handled by VFS storage.
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

        self.vfs_prefixes.push(guest_path.clone());
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
        host_path: impl AsRef<std::path::Path>,
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
    pub fn add_real_file_preopen_path(
        &mut self,
        guest_path: impl Into<String>,
        host_path: impl AsRef<std::path::Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> std::io::Result<()> {
        let host_path = host_path.as_ref();
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

        let raw_dir =
            cap_std::fs::Dir::open_ambient_dir(host_parent, cap_std::ambient_authority())?;
        let restricted =
            RestrictedDir::with_file_map(raw_dir, HashMap::from([(guest_file_name, file_name)]));
        let dir = RealDir {
            dir: restricted,
            dir_perms,
            file_perms,
            allow_blocking: false,
        };
        self.add_real_preopen(guest_parent, dir);
        Ok(())
    }

    /// Check if a path should be handled by VFS.
    pub fn is_vfs_path(&self, path: &str) -> bool {
        self.vfs_prefixes
            .iter()
            .any(|prefix| path == prefix || path.starts_with(&format!("{}/", prefix)))
    }

    /// Set whether to allow blocking the current thread for filesystem operations.
    pub fn allow_blocking_current_thread(&mut self, allow: bool) {
        self.allow_blocking_current_thread = allow;
    }
}

/// A view into the hybrid VFS state for WASI trait implementations.
pub struct HybridVfsState<'a, S: VfsStorage> {
    /// The hybrid VFS context.
    pub ctx: &'a mut HybridVfsCtx<S>,
    /// The resource table for managing descriptors.
    pub table: &'a mut ResourceTable,
}

impl<S: VfsStorage> std::fmt::Debug for HybridVfsState<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridVfsState")
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

impl<'a, S: VfsStorage> HybridVfsState<'a, S> {
    /// Create a new hybrid VFS state.
    pub fn new(ctx: &'a mut HybridVfsCtx<S>, table: &'a mut ResourceTable) -> Self {
        Self { ctx, table }
    }

    /// Check if a path should be handled by VFS.
    pub fn is_vfs_path(&self, path: &str) -> bool {
        self.ctx.is_vfs_path(path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::InMemoryStorage;

    #[test]
    fn test_is_vfs_path() {
        let storage = Arc::new(InMemoryStorage::new());
        let mut ctx = HybridVfsCtx::new(storage);
        ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::all());

        assert!(ctx.is_vfs_path("/data"));
        assert!(ctx.is_vfs_path("/data/foo"));
        assert!(ctx.is_vfs_path("/data/foo/bar"));
        assert!(!ctx.is_vfs_path("/python-stdlib"));
        assert!(!ctx.is_vfs_path("/datafile")); // Not a prefix match
        assert!(!ctx.is_vfs_path("/"));
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
