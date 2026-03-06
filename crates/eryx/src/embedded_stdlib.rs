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
    /// The stdlib is extracted to a persistent temp directory that survives
    /// for the lifetime of the process. Subsequent calls return the same path.
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

    /// Extract the embedded stdlib to a temp directory.
    fn extract() -> Result<Self, Error> {
        let temp_base = std::env::temp_dir().join("eryx-embedded");
        std::fs::create_dir_all(&temp_base)
            .map_err(|e| Error::Initialization(format!("Failed to create temp directory: {e}")))?;

        let stdlib_path = Self::extract_stdlib(&temp_base)?;
        Ok(Self { stdlib_path })
    }

    /// Extract the embedded stdlib to the temp directory.
    fn extract_stdlib(temp_dir: &Path) -> Result<PathBuf, Error> {
        let stdlib_path = temp_dir.join("python-stdlib");

        // Check if already extracted (quick validation: directory exists with some files)
        if stdlib_path.exists() {
            // Verify it looks valid (has encodings/ which is required for Python init)
            if stdlib_path.join("encodings").exists() {
                tracing::debug!(path = %stdlib_path.display(), "Using cached stdlib");
                return Ok(stdlib_path);
            }
            // Invalid, remove and re-extract
            let _ = std::fs::remove_dir_all(&stdlib_path);
        }

        tracing::info!(path = %stdlib_path.display(), "Extracting embedded Python stdlib");

        // Use tempfile to create a unique temp directory for extraction.
        // This avoids race conditions when multiple processes extract simultaneously.
        let temp_extract_dir = tempfile::TempDir::with_prefix_in("python-stdlib-", temp_dir)
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
mod tests {
    use super::*;

    #[test]
    fn embedded_stdlib_is_included() {
        assert!(
            EMBEDDED_STDLIB.len() > 1_000_000,
            "Embedded stdlib should be > 1MB"
        );
    }
}
