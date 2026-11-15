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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::embedded_stdlib::EmbeddedStdlib;
use crate::error::{Error, ErrorKind};

/// Embedded pre-compiled runtime.
const EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.cwasm"));

/// Short hash of [`EMBEDDED_RUNTIME`] for naming its cached copy: the first
/// 16 hex characters of SHA-256, computed by the build script.
const RUNTIME_CONTENT_HASH: &str = env!("ERYX_EMBEDDED_RUNTIME_HASH");

/// Paths to extracted embedded resources.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
            .map_err(|e| ErrorKind::Initialization(e.clone()).into())
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
        let content_hash = RUNTIME_CONTENT_HASH;
        let runtime_path = temp_dir.join(format!("runtime-{version}-{content_hash}.cwasm"));

        // Reuse a previous extraction only if it's ours and byte-identical:
        // the file is loaded as native code, so the name alone proves nothing.
        if matches_embedded_runtime(&runtime_path) {
            tracing::debug!(path = %runtime_path.display(), "Using cached runtime");
            return Ok(runtime_path);
        }

        // Clean up old runtime files with different hashes (same version, stale content).
        // Never our own: another process may have extracted it since the check
        // above, and removing it breaks that process's loads until we replace it.
        if let Ok(entries) = std::fs::read_dir(temp_dir) {
            let prefix = format!("runtime-{version}-");
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(&prefix)
                    && name_str.ends_with(".cwasm")
                    && entry.path() != runtime_path
                {
                    tracing::debug!(path = %entry.path().display(), "Removing stale runtime");
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        tracing::info!(path = %runtime_path.display(), "Extracting embedded runtime");
        crate::cache::publish_file(&runtime_path, EMBEDDED_RUNTIME)
            .map_err(|e| ErrorKind::Initialization(format!("Failed to write runtime file: {e}")))?;

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

/// Whether `path` is a private copy identical to [`EMBEDDED_RUNTIME`].
fn matches_embedded_runtime(path: &Path) -> bool {
    crate::cache::file_matches(path, EMBEDDED_RUNTIME)
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

    /// Processes starting together against an empty cache (#850): once any of
    /// them has the runtime, it must stay loadable while the rest extract.
    #[test]
    fn concurrent_first_use_keeps_runtime_loadable() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        crate::cache::assert_concurrent_publish_stays_loadable(|| {
            EmbeddedResources::extract_runtime(temp_dir.path()).unwrap()
        });
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
