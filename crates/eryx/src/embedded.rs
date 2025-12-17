//! Embedded resources for zero-configuration sandboxes.
//!
//! This module provides automatic extraction and caching of embedded resources:
//!
//! - **Python stdlib**: Compressed stdlib extracted to a temp directory on first use
//! - **Pre-compiled runtime**: Written to disk for mmap-based loading (10x less memory)
//!
//! # Features
//!
//! This module is only available when the `embedded` feature is enabled.
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

use crate::error::Error;

/// Embedded Python standard library (zstd-compressed tar archive).
const EMBEDDED_STDLIB: &[u8] = include_bytes!("../python-stdlib.tar.zst");

/// Embedded pre-compiled runtime.
const EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.cwasm"));

/// Paths to extracted embedded resources.
#[derive(Debug, Clone)]
pub struct EmbeddedResources {
    /// Path to the extracted Python standard library directory.
    pub stdlib_path: PathBuf,

    /// Path to the pre-compiled runtime file (for mmap loading).
    pub runtime_path: PathBuf,

    /// The temp directory (kept alive to prevent cleanup).
    #[allow(dead_code)]
    temp_dir: PathBuf,
}

impl EmbeddedResources {
    /// Get the embedded resources, extracting them on first call.
    ///
    /// Resources are extracted to a persistent temp directory that survives
    /// for the lifetime of the process. Subsequent calls return the same paths.
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

    /// Extract all embedded resources to a temp directory.
    fn extract() -> Result<Self, Error> {
        // Create a persistent temp directory
        // We use a fixed name under the system temp dir so it persists across runs
        let temp_base = std::env::temp_dir().join("eryx-embedded");
        std::fs::create_dir_all(&temp_base)
            .map_err(|e| Error::Initialization(format!("Failed to create temp directory: {e}")))?;

        let stdlib_path = Self::extract_stdlib(&temp_base)?;
        let runtime_path = Self::extract_runtime(&temp_base)?;

        Ok(Self {
            stdlib_path,
            runtime_path,
            temp_dir: temp_base,
        })
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

    /// Extract the embedded runtime to the temp directory.
    fn extract_runtime(temp_dir: &Path) -> Result<PathBuf, Error> {
        // Include version info in filename to handle upgrades
        let version = env!("CARGO_PKG_VERSION");
        let runtime_path = temp_dir.join(format!("runtime-{version}.cwasm"));

        // Check if already extracted and valid (verify size matches as basic integrity check)
        if runtime_path.exists()
            && std::fs::metadata(&runtime_path)
                .is_ok_and(|m| m.len() == EMBEDDED_RUNTIME.len() as u64)
        {
            tracing::debug!(path = %runtime_path.display(), "Using cached runtime");
            return Ok(runtime_path);
        }

        // Invalid or doesn't exist, remove any stale file and extract
        if runtime_path.exists() {
            let _ = std::fs::remove_file(&runtime_path);
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
            Err(e) if runtime_path.exists() => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_stdlib_is_included() {
        // Just verify the bytes are included
        assert!(
            EMBEDDED_STDLIB.len() > 1_000_000,
            "Embedded stdlib should be > 1MB"
        );
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
