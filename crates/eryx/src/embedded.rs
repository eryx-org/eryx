//! Embedded resources for zero-configuration sandboxes.
//!
//! This module provides automatic extraction and caching of embedded resources:
//!
//! - **Python stdlib**: Compressed stdlib extracted to the cache directory on first use
//! - **Pre-compiled runtime**: Written to disk for mmap-based loading (10x less memory)
//!
//! Both go into a per-user cache directory: `$XDG_CACHE_HOME/eryx`, else the
//! platform cache directory (`~/.cache/eryx`, `~/Library/Caches/eryx`,
//! `%LOCALAPPDATA%\eryx`), else `$TMPDIR/eryx-<uid>`. On Unix the directory is
//! created with mode `0700` and must be owned by the current user and not be
//! writable by others. An extracted runtime is compared against the embedded
//! bytes before it is used, and rewritten if it differs.
//!
//! # Features
//!
//! This module is only available when the `embedded` feature is enabled.
//! For stdlib-only embedding, see [`crate::embedded_stdlib`].
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::embedded::EmbeddedResources;
//!
//! // Get paths to embedded resources (extracts on first call)
//! let resources = EmbeddedResources::get()?;
//!
//! let sandbox = Sandbox::builder()
//!     .with_precompiled_file(&resources.runtime_path)
//!     .with_python_stdlib(&resources.stdlib_path)
//!     .build()?;
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::embedded_stdlib::EmbeddedStdlib;
use crate::error::Error;

/// Embedded pre-compiled runtime.
const EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.cwasm"));

/// Compute a short hash of the embedded runtime for cache validation.
/// Returns the first 16 hex characters of SHA-256.
fn runtime_content_hash() -> String {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(EMBEDDED_RUNTIME);
        let result = hasher.finalize();
        // Use first 8 bytes (16 hex chars) for a reasonably unique but short identifier
        result[..8].iter().map(|b| format!("{b:02x}")).collect()
    })
    .clone()
}

/// Paths to extracted embedded resources.
#[derive(Debug, Clone)]
pub struct EmbeddedResources {
    /// Path to the extracted Python standard library directory.
    pub stdlib_path: PathBuf,

    /// Path to the pre-compiled runtime file (for mmap loading).
    pub runtime_path: PathBuf,

    /// The cache directory the resources were extracted to.
    #[allow(dead_code)]
    temp_dir: PathBuf,
}

impl EmbeddedResources {
    /// Get the embedded resources, extracting them on first call.
    ///
    /// Resources are extracted to the per-user cache directory described in
    /// the [module docs](self) and reused by later processes. Subsequent calls
    /// return the same paths.
    ///
    /// # Errors
    ///
    /// Returns an error if resource extraction fails.
    pub fn get() -> Result<&'static Self, Error> {
        static RESOURCES: OnceLock<Result<EmbeddedResources, String>> = OnceLock::new();

        RESOURCES
            .get_or_init(|| Self::extract().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| Error::Initialization(e.clone()))
    }

    /// Extract all embedded resources to the cache directory.
    fn extract() -> Result<Self, Error> {
        let temp_base = crate::cache::default_cache_root()?.to_path_buf();

        // Delegate stdlib extraction to EmbeddedStdlib (shared with embedded-stdlib feature)
        let stdlib = EmbeddedStdlib::get()?;
        let stdlib_path = stdlib.path().to_path_buf();
        let runtime_path = Self::extract_runtime(&temp_base)?;

        Ok(Self {
            stdlib_path,
            runtime_path,
            temp_dir: temp_base,
        })
    }

    /// Extract the embedded runtime to the cache directory.
    fn extract_runtime(temp_dir: &Path) -> Result<PathBuf, Error> {
        // Include version AND content hash in filename to handle both version upgrades
        // and development rebuilds (where version stays the same but content changes)
        let version = env!("CARGO_PKG_VERSION");
        let content_hash = runtime_content_hash();
        let runtime_path = temp_dir.join(format!("runtime-{version}-{content_hash}.cwasm"));

        // Reuse a previous extraction only if it's ours and byte-identical:
        // the file is loaded as native code, so the name alone proves nothing.
        if matches_embedded_runtime(&runtime_path) {
            tracing::debug!(path = %runtime_path.display(), "Using cached runtime");
            return Ok(runtime_path);
        }

        // Clean up old runtime files with different hashes (same version, stale content)
        if let Ok(entries) = std::fs::read_dir(temp_dir) {
            let prefix = format!("runtime-{version}-");
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(&prefix) && name_str.ends_with(".cwasm") {
                    tracing::debug!(path = %entry.path().display(), "Removing stale runtime");
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        tracing::info!(path = %runtime_path.display(), "Extracting embedded runtime");

        // Use tempfile to create a unique temp file for writing.
        // This avoids race conditions when multiple processes extract simultaneously.
        // We use NamedTempFile so we can persist it after writing.
        let mut temp_file = tempfile::NamedTempFile::with_prefix_in("runtime-", temp_dir)
            .map_err(|e| Error::Initialization(format!("Failed to create temp file: {e}")))?;

        temp_file
            .write_all(EMBEDDED_RUNTIME)
            .map_err(|e| Error::Initialization(format!("Failed to write runtime file: {e}")))?;

        temp_file
            .as_file()
            .sync_all()
            .map_err(|e| Error::Initialization(format!("Failed to sync runtime file: {e}")))?;

        // Try to persist (rename) to the final location.
        // persist() returns the temp file back on error so we can handle it.
        match temp_file.persist(&runtime_path) {
            Ok(_) => {}
            Err(e) if matches_embedded_runtime(&runtime_path) => {
                // Another process won the race - that's fine, our temp file is cleaned up
                tracing::debug!("Runtime extracted by another process");
                // The PersistError contains the temp file, which will be cleaned up on drop
                drop(e);
            }
            Err(e) => {
                return Err(Error::Initialization(format!(
                    "Failed to persist runtime file: {}",
                    e.error
                )));
            }
        }

        Ok(runtime_path)
    }

    /// Get the path to the Python standard library.
    #[must_use]
    pub fn stdlib(&self) -> &Path {
        &self.stdlib_path
    }

    /// Get the path to the pre-compiled runtime.
    #[must_use]
    pub fn runtime(&self) -> &Path {
        &self.runtime_path
    }
}

/// Whether `path` is a regular file owned by this user, not writable by
/// others, and identical to [`EMBEDDED_RUNTIME`].
fn matches_embedded_runtime(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.is_file()
        || !crate::cache::is_private(&meta)
        || meta.len() != EMBEDDED_RUNTIME.len() as u64
    {
        return false;
    }
    let mut buf = vec![0u8; 1 << 20];
    let mut expected = EMBEDDED_RUNTIME;
    while !expected.is_empty() {
        let n = match file.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n.min(expected.len()),
        };
        if buf[..n] != expected[..n] {
            return false;
        }
        expected = &expected[n..];
    }
    true
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn extracted_runtime_must_match_embedded_bytes() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let first = EmbeddedResources::extract_runtime(temp_dir.path()).unwrap();
        assert!(matches_embedded_runtime(&first));

        // Same name, different contents: rewritten rather than reused.
        std::fs::write(&first, b"not the runtime").unwrap();
        let second = EmbeddedResources::extract_runtime(temp_dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(std::fs::read(&second).unwrap() == EMBEDDED_RUNTIME);
    }

    #[test]
    fn embedded_runtime_is_included() {
        // Just verify the bytes are included
        assert!(
            EMBEDDED_RUNTIME.len() > 1_000_000,
            "Embedded runtime should be > 1MB"
        );
    }
}
