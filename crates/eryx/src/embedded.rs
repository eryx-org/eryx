//! Embedded resources for zero-configuration sandboxes.
//!
//! This module provides automatic extraction and caching of embedded resources:
//!
//! - **Python stdlib**: Compressed stdlib extracted to a temp directory on first use
//! - **Pre-compiled runtime**: Written to disk for mmap-based loading (10x less memory)
//!
//! # Features
//!
//! - `embedded-stdlib`: Embeds the Python standard library (~2MB compressed)
//! - `embedded-runtime`: Embeds the pre-compiled WASM runtime (~XMB)
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

#[cfg(feature = "embedded-runtime")]
use std::io::Write;

use crate::error::Error;

/// Embedded Python standard library (zstd-compressed tar archive).
#[cfg(feature = "embedded-stdlib")]
const EMBEDDED_STDLIB: &[u8] = include_bytes!("../python-stdlib.tar.zst");

/// Embedded pre-compiled runtime.
#[cfg(feature = "embedded-runtime")]
const EMBEDDED_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.cwasm"));

/// Paths to extracted embedded resources.
#[derive(Debug, Clone)]
pub struct EmbeddedResources {
    /// Path to the extracted Python standard library directory.
    #[cfg(feature = "embedded-stdlib")]
    pub stdlib_path: PathBuf,

    /// Path to the pre-compiled runtime file (for mmap loading).
    #[cfg(feature = "embedded-runtime")]
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

        #[cfg(feature = "embedded-stdlib")]
        let stdlib_path = Self::extract_stdlib(&temp_base)?;

        #[cfg(feature = "embedded-runtime")]
        let runtime_path = Self::extract_runtime(&temp_base)?;

        Ok(Self {
            #[cfg(feature = "embedded-stdlib")]
            stdlib_path,
            #[cfg(feature = "embedded-runtime")]
            runtime_path,
            temp_dir: temp_base,
        })
    }

    /// Extract the embedded stdlib to the temp directory.
    #[cfg(feature = "embedded-stdlib")]
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

        // Decompress zstd
        let decoder = zstd::Decoder::new(EMBEDDED_STDLIB)
            .map_err(|e| Error::Initialization(format!("Failed to create zstd decoder: {e}")))?;

        // Extract tar archive
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(temp_dir)
            .map_err(|e| Error::Initialization(format!("Failed to extract stdlib archive: {e}")))?;

        // Verify extraction
        if !stdlib_path.join("encodings").exists() {
            return Err(Error::Initialization(
                "Stdlib extraction failed: encodings/ not found".to_string(),
            ));
        }

        Ok(stdlib_path)
    }

    /// Extract the embedded runtime to the temp directory.
    #[cfg(feature = "embedded-runtime")]
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

        // Write to a temp file first, then rename for atomicity
        let temp_path = runtime_path.with_extension("cwasm.tmp");
        let mut file = std::fs::File::create(&temp_path)
            .map_err(|e| Error::Initialization(format!("Failed to create runtime file: {e}")))?;

        file.write_all(EMBEDDED_RUNTIME)
            .map_err(|e| Error::Initialization(format!("Failed to write runtime file: {e}")))?;

        file.sync_all()
            .map_err(|e| Error::Initialization(format!("Failed to sync runtime file: {e}")))?;

        drop(file);

        std::fs::rename(&temp_path, &runtime_path)
            .map_err(|e| Error::Initialization(format!("Failed to rename runtime file: {e}")))?;

        Ok(runtime_path)
    }

    /// Get the path to the Python standard library.
    ///
    /// # Panics
    ///
    /// Panics if the `embedded-stdlib` feature is not enabled.
    #[cfg(feature = "embedded-stdlib")]
    #[must_use]
    pub fn stdlib(&self) -> &Path {
        &self.stdlib_path
    }

    /// Get the path to the pre-compiled runtime.
    ///
    /// # Panics
    ///
    /// Panics if the `embedded-runtime` feature is not enabled.
    #[cfg(feature = "embedded-runtime")]
    #[must_use]
    pub fn runtime(&self) -> &Path {
        &self.runtime_path
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    #[cfg(feature = "embedded-stdlib")]
    fn embedded_stdlib_is_included() {
        // Just verify the bytes are included
        assert!(
            EMBEDDED_STDLIB.len() > 1_000_000,
            "Embedded stdlib should be > 1MB"
        );
    }

    #[test]
    #[cfg(feature = "embedded-runtime")]
    fn embedded_runtime_is_included() {
        // Just verify the bytes are included
        assert!(
            EMBEDDED_RUNTIME.len() > 1_000_000,
            "Embedded runtime should be > 1MB"
        );
    }
}
