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

use crate::link_order::BaseLibraries;

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

    // wit-component 0.257 turned `Linker` into a `&mut self` builder and moved
    // `validate`/`adapter` onto the inner `ComponentEncoder`.
    let mut linker = Linker::default();
    linker.use_built_in_libdl(true);
    linker.encoder().validate(true);

    let libs = BaseLibraries {
        wasi_clocks: &wasi_clocks,
        wasi_signal: &wasi_signal,
        wasi_mman: &wasi_mman,
        wasi_getpid: &wasi_getpid,
        libc: &libc,
        libcxxabi: &libcxxabi,
        libcxx: &libcxx,
        libpython: &libpython,
        runtime: &runtime,
        bindings: &bindings,
    };
    for (name, bytes) in libs.in_link_order() {
        linker
            .library(name, bytes, false)
            .map_err(|e| LinkError::Library(name.into(), e.to_string()))?;
    }

    // Add user's native extensions (dl_openable = true for dlopen/dlsym)
    for ext in extensions {
        linker
            .library(&ext.name, &ext.bytes, true)
            .map_err(|e| LinkError::Extension(ext.name.clone(), e.to_string()))?;
    }

    // Add WASI adapter
    linker
        .encoder()
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
#[non_exhaustive]
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
