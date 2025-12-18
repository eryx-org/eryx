//! Package extraction and management.
//!
//! This module handles extracting Python packages from various formats:
//! - Standard wheels (`.whl`) - zip files with Python code and native extensions
//! - wasi-wheels tar.gz - archives from the wasi-wheels project
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::Sandbox;
//!
//! // Pure Python package - works with embedded runtime
//! let sandbox = Sandbox::builder()
//!     .with_embedded_runtime()
//!     .with_package("/path/to/requests-2.31.0-py3-none-any.whl")?
//!     .build()?;
//!
//! // Package with native extensions - auto late-links
//! let sandbox = Sandbox::builder()
//!     .with_package("/path/to/numpy-wasi.tar.gz")?
//!     .with_cache_dir("/tmp/cache")?
//!     .build()?;
//! ```

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Detected package format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFormat {
    /// Standard Python wheel (.whl) - a zip file.
    Wheel,
    /// Tarball (.tar.gz, .tgz) - used by wasi-wheels.
    TarGz,
    /// Plain directory containing Python files.
    Directory,
}

impl PackageFormat {
    /// Detect the package format from the file path.
    pub fn detect(path: &Path) -> Option<Self> {
        if path.is_dir() {
            return Some(Self::Directory);
        }

        let name = path.file_name()?.to_str()?;
        let lower = name.to_lowercase();

        if lower.ends_with(".whl") {
            Some(Self::Wheel)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else {
            None
        }
    }
}

/// An extracted package ready for use in a sandbox.
#[derive(Debug)]
pub struct ExtractedPackage {
    /// Package name (e.g., "numpy").
    pub name: String,
    /// Path to the extracted Python files directory.
    /// This should be mounted at `/site-packages`.
    pub python_path: PathBuf,
    /// Native extensions (.so files) with their dlopen paths.
    /// Empty for pure-Python packages.
    pub native_extensions: Vec<NativeExtension>,
    /// Whether this package has native extensions.
    pub has_native_extensions: bool,
    /// Temp directory handle (kept alive to prevent cleanup).
    #[allow(dead_code)]
    temp_dir: Option<tempfile::TempDir>,
}

/// A native extension (.so file) found in a package.
#[derive(Debug, Clone)]
pub struct NativeExtension {
    /// Relative path within the package (e.g., "numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so").
    /// This will be prefixed with the mount path when building the sandbox.
    pub relative_path: String,
    /// The raw bytes of the .so file.
    pub bytes: Vec<u8>,
}

impl ExtractedPackage {
    /// Extract a package from a file path.
    ///
    /// The format is auto-detected from the file extension.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The format cannot be detected
    /// - The archive cannot be read or extracted
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();

        let format = PackageFormat::detect(path).ok_or_else(|| {
            Error::Initialization(format!(
                "Cannot detect package format for '{}'. Expected .whl, .tar.gz, or directory.",
                path.display()
            ))
        })?;

        match format {
            PackageFormat::Wheel => Self::from_wheel(path),
            PackageFormat::TarGz => Self::from_tar_gz(path),
            PackageFormat::Directory => Self::from_directory(path),
        }
    }

    /// Extract a package from raw bytes.
    ///
    /// The format is specified explicitly since it cannot be detected from bytes alone.
    /// The `name_hint` is used for the package name if one cannot be detected from
    /// the archive contents (e.g., "numpy" or "requests-2.31.0").
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::package::{ExtractedPackage, PackageFormat};
    ///
    /// // Load from bytes (e.g., downloaded from a URL)
    /// let bytes = download_package("https://example.com/numpy-wasi.tar.gz").await?;
    /// let package = ExtractedPackage::from_bytes(&bytes, PackageFormat::TarGz, "numpy")?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The format is `Directory` (directories cannot be loaded from bytes)
    /// - The archive cannot be read or extracted
    pub fn from_bytes(
        bytes: &[u8],
        format: PackageFormat,
        name_hint: impl Into<String>,
    ) -> Result<Self, Error> {
        let name_hint = name_hint.into();

        match format {
            PackageFormat::Wheel => Self::from_wheel_bytes(bytes, &name_hint),
            PackageFormat::TarGz => Self::from_tar_gz_bytes(bytes, &name_hint),
            PackageFormat::Directory => Err(Error::Initialization(
                "Cannot load directory package from bytes. Use from_path() instead.".to_string(),
            )),
        }
    }

    /// Extract a wheel from raw bytes.
    fn from_wheel_bytes(bytes: &[u8], name_hint: &str) -> Result<Self, Error> {
        let cursor = std::io::Cursor::new(bytes);
        Self::from_wheel_reader(cursor, name_hint)
    }

    /// Extract a tar.gz from raw bytes.
    fn from_tar_gz_bytes(bytes: &[u8], name_hint: &str) -> Result<Self, Error> {
        let cursor = std::io::Cursor::new(bytes);
        let decoder = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(decoder);

        // Create temp directory for extraction
        let temp_dir = tempfile::TempDir::new()
            .map_err(|e| Error::Initialization(format!("Failed to create temp dir: {e}")))?;

        let extract_path = temp_dir.path();

        archive
            .unpack(extract_path)
            .map_err(|e| Error::Initialization(format!("Failed to extract tar.gz: {e}")))?;

        // Find native extensions and package name
        let (native_extensions, name) = Self::scan_for_extensions(extract_path)?;

        // Use name hint for package name if not found
        let name = name.unwrap_or_else(|| {
            // Handle names like "numpy-wasi" -> "numpy"
            name_hint.split('-').next().unwrap_or(name_hint).to_string()
        });

        let has_native_extensions = !native_extensions.is_empty();

        Ok(Self {
            name,
            python_path: extract_path.to_path_buf(),
            native_extensions,
            has_native_extensions,
            temp_dir: Some(temp_dir),
        })
    }

    /// Extract a standard wheel (.whl) file.
    fn from_wheel(path: &Path) -> Result<Self, Error> {
        let file = std::fs::File::open(path)
            .map_err(|e| Error::Initialization(format!("Failed to open wheel: {e}")))?;

        // Extract name hint from path (e.g., "requests-2.31.0-py3-none-any.whl" -> "requests")
        let name_hint = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.split('-').next().unwrap_or(s))
            .unwrap_or("unknown");

        Self::from_wheel_reader(file, name_hint)
    }

    /// Extract a wheel from a reader.
    fn from_wheel_reader<R: Read + Seek>(reader: R, name_hint: &str) -> Result<Self, Error> {
        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| Error::Initialization(format!("Failed to read wheel archive: {e}")))?;

        // Create temp directory for extraction
        let temp_dir = tempfile::TempDir::new()
            .map_err(|e| Error::Initialization(format!("Failed to create temp dir: {e}")))?;

        let extract_path = temp_dir.path();

        // Extract all files
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| Error::Initialization(format!("Failed to read archive entry: {e}")))?;

            let outpath = extract_path.join(file.name());

            if file.is_dir() {
                std::fs::create_dir_all(&outpath).map_err(|e| {
                    Error::Initialization(format!("Failed to create directory: {e}"))
                })?;
            } else {
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        Error::Initialization(format!("Failed to create parent directory: {e}"))
                    })?;
                }

                let mut outfile = std::fs::File::create(&outpath)
                    .map_err(|e| Error::Initialization(format!("Failed to create file: {e}")))?;

                std::io::copy(&mut file, &mut outfile)
                    .map_err(|e| Error::Initialization(format!("Failed to extract file: {e}")))?;
            }
        }

        // Find native extensions and package name
        let (native_extensions, name) = Self::scan_for_extensions(extract_path)?;

        // Use name hint for package name if not found
        let name = name.unwrap_or_else(|| {
            // Handle names like "requests-2.31.0" -> "requests"
            name_hint.split('-').next().unwrap_or(name_hint).to_string()
        });

        let has_native_extensions = !native_extensions.is_empty();

        Ok(Self {
            name,
            python_path: extract_path.to_path_buf(),
            native_extensions,
            has_native_extensions,
            temp_dir: Some(temp_dir),
        })
    }

    /// Extract a tar.gz archive (wasi-wheels format).
    fn from_tar_gz(path: &Path) -> Result<Self, Error> {
        let file = std::fs::File::open(path)
            .map_err(|e| Error::Initialization(format!("Failed to open tar.gz: {e}")))?;

        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);

        // Create temp directory for extraction
        let temp_dir = tempfile::TempDir::new()
            .map_err(|e| Error::Initialization(format!("Failed to create temp dir: {e}")))?;

        let extract_path = temp_dir.path();

        archive
            .unpack(extract_path)
            .map_err(|e| Error::Initialization(format!("Failed to extract tar.gz: {e}")))?;

        // Find native extensions and package name
        let (native_extensions, name) = Self::scan_for_extensions(extract_path)?;

        // Use source filename for package name if not found
        let name = name.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| {
                    // Handle names like "numpy-wasi.tar" -> "numpy"
                    s.split('-').next().unwrap_or(s).to_string()
                })
                .unwrap_or_else(|| "unknown".to_string())
        });

        let has_native_extensions = !native_extensions.is_empty();

        Ok(Self {
            name,
            python_path: extract_path.to_path_buf(),
            native_extensions,
            has_native_extensions,
            temp_dir: Some(temp_dir),
        })
    }

    /// Use a directory directly (no extraction needed).
    fn from_directory(path: &Path) -> Result<Self, Error> {
        if !path.is_dir() {
            return Err(Error::Initialization(format!(
                "Path is not a directory: {}",
                path.display()
            )));
        }

        // Find native extensions and package name
        let (native_extensions, name) = Self::scan_for_extensions(path)?;

        // Use directory name as package name if not found
        let name = name.unwrap_or_else(|| {
            path.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        });

        let has_native_extensions = !native_extensions.is_empty();

        Ok(Self {
            name,
            python_path: path.to_path_buf(),
            native_extensions,
            has_native_extensions,
            temp_dir: None,
        })
    }

    /// Scan a directory for native extensions (.so files) and try to detect package name.
    fn scan_for_extensions(dir: &Path) -> Result<(Vec<NativeExtension>, Option<String>), Error> {
        let mut extensions = Vec::new();
        let mut package_name = None;

        // Walk the directory tree
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry
                .map_err(|e| Error::Initialization(format!("Failed to walk directory: {e}")))?;

            let path = entry.path();

            // Look for .so files (native extensions)
            if path.extension().is_some_and(|ext| ext == "so") {
                // Compute the relative path within the package directory
                let relative = path.strip_prefix(dir).map_err(|e| {
                    Error::Initialization(format!("Failed to compute relative path: {e}"))
                })?;

                let relative_path = relative.display().to_string();

                let bytes = std::fs::read(path)
                    .map_err(|e| Error::Initialization(format!("Failed to read .so file: {e}")))?;

                extensions.push(NativeExtension {
                    relative_path,
                    bytes,
                });
            }

            // Try to detect package name from __init__.py in top-level directories
            if package_name.is_none()
                && path.file_name().is_some_and(|n| n == "__init__.py")
                && path.parent().is_some_and(|p| p.parent() == Some(dir))
            {
                // This is a top-level package
                package_name = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string());
            }
        }

        Ok((extensions, package_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_wheel_format() {
        assert_eq!(
            PackageFormat::detect(Path::new("requests-2.31.0-py3-none-any.whl")),
            Some(PackageFormat::Wheel)
        );
    }

    #[test]
    fn detect_tar_gz_format() {
        assert_eq!(
            PackageFormat::detect(Path::new("numpy-wasi.tar.gz")),
            Some(PackageFormat::TarGz)
        );
        assert_eq!(
            PackageFormat::detect(Path::new("package.tgz")),
            Some(PackageFormat::TarGz)
        );
    }

    #[test]
    fn detect_unknown_format() {
        assert_eq!(PackageFormat::detect(Path::new("file.txt")), None);
        assert_eq!(PackageFormat::detect(Path::new("file.zip")), None);
    }
}
