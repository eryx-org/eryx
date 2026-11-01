//! Embedded Python standard library for use with custom runtimes.
//!
//! This module provides extraction and caching of the embedded Python stdlib
//! without requiring the full embedded runtime (~30MB+ savings).
//!
//! # Features
//!
//! This module is available when the `embedded-stdlib` feature is enabled.
//! The full `embedded` feature implies `embedded-stdlib`.
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::embedded_stdlib::EmbeddedStdlib;
//!
//! // Get path to embedded stdlib (extracts on first call)
//! let stdlib = EmbeddedStdlib::get()?;
//!
//! let sandbox = unsafe {
//!     Sandbox::builder()
//!         .with_precompiled_file("/path/to/custom-runtime.cwasm")
//!         .with_python_stdlib(stdlib.path())
//!         .build()?
//! };
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::error::Error;

/// Embedded Python standard library (zstd-compressed tar archive).
const EMBEDDED_STDLIB: &[u8] = include_bytes!("../python-stdlib.tar.zst");

/// Paths to the extracted embedded stdlib.
#[derive(Debug, Clone)]
pub struct EmbeddedStdlib {
    /// Path to the extracted Python standard library directory.
    stdlib_path: PathBuf,
}

impl EmbeddedStdlib {
    /// Get the embedded stdlib, extracting it on first call.
    ///
    /// The stdlib is extracted once into the per-user eryx cache directory
    /// (`$XDG_CACHE_HOME/eryx` or the platform cache directory, falling back
    /// to `$TMPDIR/eryx-<uid>`) and reused by later processes. Subsequent calls return the
    /// same path.
    ///
    /// # Errors
    ///
    /// Returns an error if extraction fails.
    pub fn get() -> Result<&'static Self, Error> {
        static STDLIB: OnceLock<Result<EmbeddedStdlib, String>> = OnceLock::new();

        STDLIB
            .get_or_init(|| Self::extract().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| Error::Initialization(e.clone()))
    }

    /// Extract the embedded stdlib to the cache directory.
    fn extract() -> Result<Self, Error> {
        let cache_root = crate::cache::default_cache_root()?;
        let stdlib_path = Self::extract_stdlib(cache_root)?;
        Ok(Self { stdlib_path })
    }

    /// Extract the embedded stdlib into `cache_dir`.
    fn extract_stdlib(cache_dir: &Path) -> Result<PathBuf, Error> {
        // The cache directory outlives upgrades, so key the extracted copy by
        // version and content rather than reusing whatever is there.
        // Hashed by the build script, so startup doesn't hash the archive
        let content_hash = env!("ERYX_EMBEDDED_STDLIB_HASH");
        let stdlib_path = cache_dir.join(format!(
            "python-stdlib-{}-{content_hash}",
            env!("CARGO_PKG_VERSION")
        ));

        // Reuse a previous extraction if it's ours and looks complete (has
        // encodings/, which Python needs to start).
        if let Ok(meta) = std::fs::symlink_metadata(&stdlib_path) {
            if meta.is_dir()
                && crate::cache::is_private(&meta)
                && stdlib_path.join("encodings").exists()
            {
                tracing::debug!(path = %stdlib_path.display(), "Using cached stdlib");
                return Ok(stdlib_path);
            }
            // Invalid, remove and re-extract
            let _ = std::fs::remove_dir_all(&stdlib_path);
            let _ = std::fs::remove_file(&stdlib_path);
        }

        tracing::info!(path = %stdlib_path.display(), "Extracting embedded Python stdlib");

        // Use tempfile to create a unique temp directory for extraction.
        // This avoids race conditions when multiple processes extract simultaneously.
        let temp_extract_dir = tempfile::TempDir::with_prefix_in(".python-stdlib-", cache_dir)
            .map_err(|e| {
                Error::Initialization(format!("Failed to create temp extract directory: {e}"))
            })?;

        // Decompress zstd
        let decoder = zstd::Decoder::new(EMBEDDED_STDLIB)
            .map_err(|e| Error::Initialization(format!("Failed to create zstd decoder: {e}")))?;

        // Extract tar archive to temp directory
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(temp_extract_dir.path())
            .map_err(|e| Error::Initialization(format!("Failed to extract stdlib archive: {e}")))?;

        // The archive extracts to python-stdlib/ inside the temp dir
        let extracted_stdlib = temp_extract_dir.path().join("python-stdlib");

        // Verify extraction
        if !extracted_stdlib.join("encodings").exists() {
            return Err(Error::Initialization(
                "Stdlib extraction failed: encodings/ not found".to_string(),
            ));
        }

        // Keep the tree private to this user whatever modes the archive carries.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&extracted_stdlib, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| {
                    Error::Initialization(format!("Failed to set stdlib permissions: {e}"))
                })?;
        }

        // Atomically rename to final location
        match std::fs::rename(&extracted_stdlib, &stdlib_path) {
            Ok(()) => {
                // TempDir will clean up the now-empty temp extract dir on drop
            }
            Err(_) if stdlib_path.join("encodings").exists() => {
                // Another process won the race - that's fine, TempDir cleans up on drop
                tracing::debug!("Stdlib extracted by another process");
            }
            Err(e) => {
                return Err(Error::Initialization(format!(
                    "Failed to rename stdlib directory: {e}"
                )));
            }
        }

        Ok(stdlib_path)
    }

    /// Get the path to the Python standard library.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.stdlib_path
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn stdlib_writable_by_others_is_reextracted() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let stdlib = EmbeddedStdlib::extract_stdlib(temp_dir.path()).unwrap();
        assert!(stdlib.join("encodings").exists());

        let marker = stdlib.join("planted.py");
        std::fs::write(&marker, b"").unwrap();
        std::fs::set_permissions(&stdlib, std::fs::Permissions::from_mode(0o777)).unwrap();

        let again = EmbeddedStdlib::extract_stdlib(temp_dir.path()).unwrap();
        assert_eq!(stdlib, again);
        assert!(!marker.exists());
        let mode = std::fs::metadata(&again).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn embedded_stdlib_is_included() {
        assert!(
            EMBEDDED_STDLIB.len() > 1_000_000,
            "Embedded stdlib should be > 1MB"
        );
    }
}
