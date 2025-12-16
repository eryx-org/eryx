//! Late-linking support for native Python extensions.
//!
//! This module provides functionality to link native Python extensions (.so files)
//! into the eryx runtime at sandbox creation time using the wit-component Linker.
//!
//! # Architecture
//!
//! The base libraries (libc, libpython, etc.) are embedded in the crate and combined
//! with user-provided native extensions using shared-everything dynamic linking.
//! Since true runtime dynamic linking isn't yet supported in the component model,
//! we re-link the component when native extensions are added and cache the result.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                    Base Libraries (embedded)                         │
//! │   libc.so, libpython3.14.so, liberyx_runtime.so, etc.               │
//! └─────────────────────────────────────────────────────────────────────┘
//!                               +
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                    Native Extensions (user-provided)                 │
//! │   numpy/*.cpython-314-wasm32-wasi.so, etc.                          │
//! └─────────────────────────────────────────────────────────────────────┘
//!                               ↓
//!                     wit_component::Linker
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                    Linked Component                                  │
//! │   Complete WASM component with native extensions available          │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

use std::io::Cursor;

use sha2::{Digest, Sha256};

/// A native extension to be linked into the component.
#[derive(Debug, Clone)]
pub struct NativeExtension {
    /// The name of the .so file (e.g., "_multiarray_umath.cpython-314-wasm32-wasi.so")
    pub name: String,
    /// The raw WASM bytes of the .so file
    pub bytes: Vec<u8>,
}

impl NativeExtension {
    /// Create a new native extension.
    #[must_use]
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            bytes,
        }
    }
}

/// Metadata about a Python wheel.
#[derive(Debug, Clone)]
pub struct WheelInfo {
    /// Package name (e.g., "numpy")
    pub name: String,
    /// Package version (e.g., "1.26.0")
    pub version: String,
    /// Python files in the wheel (path, contents)
    pub python_files: Vec<(String, Vec<u8>)>,
    /// Native extensions (.so files) if any
    pub native_extensions: Vec<NativeExtension>,
}

impl WheelInfo {
    /// Returns true if this wheel contains native extensions.
    #[must_use]
    pub fn has_native_extensions(&self) -> bool {
        !self.native_extensions.is_empty()
    }
}

/// Parse a Python wheel (ZIP file) and extract its contents.
///
/// # Errors
///
/// Returns an error if the wheel cannot be parsed.
pub fn parse_wheel(wheel_bytes: &[u8]) -> Result<WheelInfo, WheelParseError> {
    use std::io::Read;

    let reader = Cursor::new(wheel_bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| WheelParseError::InvalidZip(e.to_string()))?;

    let mut python_files = Vec::new();
    let mut native_extensions = Vec::new();
    let mut name = String::new();
    let mut version = String::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| WheelParseError::InvalidZip(e.to_string()))?;

        let file_name = file.name().to_string();

        // Extract package info from METADATA
        if file_name.ends_with(".dist-info/METADATA") {
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|e| WheelParseError::ReadError(e.to_string()))?;

            for line in contents.lines() {
                if let Some(n) = line.strip_prefix("Name: ") {
                    name = n.to_string();
                } else if let Some(v) = line.strip_prefix("Version: ") {
                    version = v.to_string();
                }
            }
        }
        // Check for native extensions (.so files for WASI)
        else if file_name.ends_with(".so") && file_name.contains("wasm32-wasi") {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| WheelParseError::ReadError(e.to_string()))?;

            // Extract just the filename from the path
            let so_name = file_name
                .rsplit('/')
                .next()
                .unwrap_or(&file_name)
                .to_string();

            native_extensions.push(NativeExtension {
                name: so_name,
                bytes,
            });
        }
        // Collect Python files
        else if file_name.ends_with(".py") || file_name.ends_with(".pyi") {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| WheelParseError::ReadError(e.to_string()))?;

            python_files.push((file_name, bytes));
        }
    }

    Ok(WheelInfo {
        name,
        version,
        python_files,
        native_extensions,
    })
}

/// Errors that can occur when parsing a wheel.
#[derive(Debug, Clone)]
pub enum WheelParseError {
    /// The wheel is not a valid ZIP file.
    InvalidZip(String),
    /// Failed to read a file from the wheel.
    ReadError(String),
}

impl std::fmt::Display for WheelParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidZip(e) => write!(f, "invalid ZIP file: {e}"),
            Self::ReadError(e) => write!(f, "failed to read file: {e}"),
        }
    }
}

impl std::error::Error for WheelParseError {}

/// Base libraries embedded in the crate (compressed with zstd).
pub mod base_libraries {
    /// libc.so - C standard library
    pub const LIBC: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/libs/libc.so.zst"));

    /// libc++.so - C++ standard library
    pub const LIBCXX: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/libs/libc++.so.zst"));

    /// libc++abi.so - C++ ABI library
    pub const LIBCXXABI: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libc++abi.so.zst"
    ));

    /// libpython3.14.so - Python interpreter
    pub const LIBPYTHON: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libpython3.14.so.zst"
    ));

    /// libwasi-emulated-mman.so - WASI memory management emulation
    pub const LIBWASI_EMULATED_MMAN: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libwasi-emulated-mman.so.zst"
    ));

    /// libwasi-emulated-process-clocks.so - WASI process clocks emulation
    pub const LIBWASI_EMULATED_PROCESS_CLOCKS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libwasi-emulated-process-clocks.so.zst"
    ));

    /// libwasi-emulated-getpid.so - WASI getpid emulation
    pub const LIBWASI_EMULATED_GETPID: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libwasi-emulated-getpid.so.zst"
    ));

    /// libwasi-emulated-signal.so - WASI signal emulation
    pub const LIBWASI_EMULATED_SIGNAL: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/libwasi-emulated-signal.so.zst"
    ));

    /// WASI adapter (preview1 to preview2)
    pub const WASI_ADAPTER: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libs/wasi_snapshot_preview1.reactor.wasm.zst"
    ));

    /// liberyx_runtime.so - Our custom runtime (built during cargo build)
    pub const LIBERYX_RUNTIME: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/liberyx_runtime.so.zst"));

    /// liberyx_bindings.so - WIT bindings for our runtime (built during cargo build)
    pub const LIBERYX_BINDINGS: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/liberyx_bindings.so.zst"));
}

/// Compute a cache key for a set of native extensions.
///
/// The key is a SHA-256 hash of all extension names and contents,
/// sorted by name for determinism.
#[must_use]
pub fn compute_cache_key(extensions: &[NativeExtension]) -> [u8; 32] {
    let mut hasher = Sha256::new();

    // Sort extensions by name for deterministic hashing
    let mut sorted: Vec<_> = extensions.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for ext in sorted {
        hasher.update(ext.name.as_bytes());
        hasher.update((ext.bytes.len() as u64).to_le_bytes());
        hasher.update(&ext.bytes);
    }

    hasher.finalize().into()
}

/// Link native extensions with the base libraries to create a new component.
///
/// This uses our custom eryx-wasm-runtime instead of componentize-py's runtime.
///
/// # Arguments
///
/// * `extensions` - Native extensions to include (will be dl_openable)
///
/// # Returns
///
/// The linked component as WASM bytes.
///
/// # Errors
///
/// Returns an error if linking fails.
pub fn link_with_extensions(extensions: &[NativeExtension]) -> Result<Vec<u8>, LinkError> {
    use wit_component::Linker;

    // Decompress base libraries
    let libc = decompress_zstd(base_libraries::LIBC)?;
    let libcxx = decompress_zstd(base_libraries::LIBCXX)?;
    let libcxxabi = decompress_zstd(base_libraries::LIBCXXABI)?;
    let libpython = decompress_zstd(base_libraries::LIBPYTHON)?;
    let wasi_mman = decompress_zstd(base_libraries::LIBWASI_EMULATED_MMAN)?;
    let wasi_clocks = decompress_zstd(base_libraries::LIBWASI_EMULATED_PROCESS_CLOCKS)?;
    let wasi_getpid = decompress_zstd(base_libraries::LIBWASI_EMULATED_GETPID)?;
    let wasi_signal = decompress_zstd(base_libraries::LIBWASI_EMULATED_SIGNAL)?;
    let adapter = decompress_zstd(base_libraries::WASI_ADAPTER)?;
    let runtime = decompress_zstd(base_libraries::LIBERYX_RUNTIME)?;
    let bindings = decompress_zstd(base_libraries::LIBERYX_BINDINGS)?;

    let mut linker = Linker::default().validate(true).use_built_in_libdl(true);

    // Add base libraries (order matters for symbol resolution)
    linker = linker
        // WASI emulation libraries
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)
        .map_err(|e| {
            LinkError::Library("libwasi-emulated-process-clocks.so".into(), e.to_string())
        })?
        .library("libwasi-emulated-signal.so", &wasi_signal, false)
        .map_err(|e| LinkError::Library("libwasi-emulated-signal.so".into(), e.to_string()))?
        .library("libwasi-emulated-mman.so", &wasi_mman, false)
        .map_err(|e| LinkError::Library("libwasi-emulated-mman.so".into(), e.to_string()))?
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)
        .map_err(|e| LinkError::Library("libwasi-emulated-getpid.so".into(), e.to_string()))?
        // C/C++ runtime
        .library("libc.so", &libc, false)
        .map_err(|e| LinkError::Library("libc.so".into(), e.to_string()))?
        .library("libc++abi.so", &libcxxabi, false)
        .map_err(|e| LinkError::Library("libc++abi.so".into(), e.to_string()))?
        .library("libc++.so", &libcxx, false)
        .map_err(|e| LinkError::Library("libc++.so".into(), e.to_string()))?
        // Python
        .library("libpython3.14.so", &libpython, false)
        .map_err(|e| LinkError::Library("libpython3.14.so".into(), e.to_string()))?
        // Our runtime and bindings
        .library("liberyx_runtime.so", &runtime, false)
        .map_err(|e| LinkError::Library("liberyx_runtime.so".into(), e.to_string()))?
        .library("liberyx_bindings.so", &bindings, false)
        .map_err(|e| LinkError::Library("liberyx_bindings.so".into(), e.to_string()))?;

    // Add user's native extensions (dl_openable = true for dlopen/dlsym)
    for ext in extensions {
        linker = linker
            .library(&ext.name, &ext.bytes, true)
            .map_err(|e| LinkError::Extension(ext.name.clone(), e.to_string()))?;
    }

    // Add WASI adapter
    linker = linker
        .adapter("wasi_snapshot_preview1", &adapter)
        .map_err(|e| LinkError::Adapter(e.to_string()))?;

    linker
        .encode()
        .map_err(|e| LinkError::Encode(e.to_string()))
}

fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>, LinkError> {
    zstd::decode_all(Cursor::new(data)).map_err(|e| LinkError::Decompress(e.to_string()))
}

/// Errors that can occur during linking.
#[derive(Debug, Clone)]
pub enum LinkError {
    /// Failed to add a base library.
    Library(String, String),
    /// Failed to add a native extension.
    Extension(String, String),
    /// Failed to add the WASI adapter.
    Adapter(String),
    /// Failed to encode the final component.
    Encode(String),
    /// Failed to decompress a library.
    Decompress(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Library(name, e) => write!(f, "failed to add base library {name}: {e}"),
            Self::Extension(name, e) => write!(f, "failed to add extension {name}: {e}"),
            Self::Adapter(e) => write!(f, "failed to add WASI adapter: {e}"),
            Self::Encode(e) => write!(f, "failed to encode component: {e}"),
            Self::Decompress(e) => write!(f, "failed to decompress library: {e}"),
        }
    }
}

impl std::error::Error for LinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_determinism() {
        let ext1 = NativeExtension::new("a.so", vec![1, 2, 3]);
        let ext2 = NativeExtension::new("b.so", vec![4, 5, 6]);

        // Same extensions in different order should produce same key
        let key1 = compute_cache_key(&[ext1.clone(), ext2.clone()]);
        let key2 = compute_cache_key(&[ext2, ext1]);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_wheel_parse_error_display() {
        let err = WheelParseError::InvalidZip("test error".to_string());
        assert!(err.to_string().contains("test error"));
    }
}
