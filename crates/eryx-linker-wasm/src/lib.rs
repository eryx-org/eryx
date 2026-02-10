//! Browser-side WASM linker for eryx native extensions.
//!
//! This crate compiles to `wasm32-unknown-unknown` via `wasm-bindgen` and exposes
//! a single `link_extensions()` function that mirrors the logic in
//! `eryx-runtime/src/linker.rs`. It takes already-decompressed base libraries
//! (decompressed in JS via fzstd) and user-provided native extension `.so` files,
//! and returns a linked WASM component.

#![deny(unsafe_code)]

use wasm_bindgen::prelude::*;
use wit_component::Linker;

/// A native extension to be linked.
///
/// Passed from JS as an object with `name` (string) and `bytes` (Uint8Array).
#[wasm_bindgen]
#[allow(missing_debug_implementations)]
pub struct NativeExtension {
    name: String,
    bytes: Vec<u8>,
}

#[wasm_bindgen]
impl NativeExtension {
    /// Create a new native extension.
    #[wasm_bindgen(constructor)]
    pub fn new(name: String, bytes: Vec<u8>) -> Self {
        Self { name, bytes }
    }
}

/// Link native extensions with decompressed base libraries to produce a WASM component.
///
/// Base libraries must be passed as already-decompressed bytes (JS handles zstd
/// decompression via fzstd). The order of base libraries matches
/// `eryx-runtime/src/linker.rs:268-330`.
///
/// # Arguments
///
/// * `libc` - libc.so bytes
/// * `libcxx` - libc++.so bytes
/// * `libcxxabi` - libc++abi.so bytes
/// * `libpython` - libpython3.14.so bytes
/// * `wasi_mman` - libwasi-emulated-mman.so bytes
/// * `wasi_clocks` - libwasi-emulated-process-clocks.so bytes
/// * `wasi_getpid` - libwasi-emulated-getpid.so bytes
/// * `wasi_signal` - libwasi-emulated-signal.so bytes
/// * `adapter` - wasi_snapshot_preview1.reactor.wasm bytes
/// * `runtime` - liberyx_runtime.so bytes
/// * `bindings` - liberyx_bindings.so bytes
/// * `extensions` - Array of NativeExtension objects
///
/// # Returns
///
/// The linked WASM component as a `Uint8Array`.
///
/// # Errors
///
/// Throws a JS error if linking fails.
#[wasm_bindgen(js_name = linkExtensions)]
#[allow(clippy::too_many_arguments)]
pub fn link_extensions(
    libc: &[u8],
    libcxx: &[u8],
    libcxxabi: &[u8],
    libpython: &[u8],
    wasi_mman: &[u8],
    wasi_clocks: &[u8],
    wasi_getpid: &[u8],
    wasi_signal: &[u8],
    adapter: &[u8],
    runtime: &[u8],
    bindings: &[u8],
    extensions: Vec<NativeExtension>,
) -> Result<Vec<u8>, JsError> {
    let mut linker = Linker::default().validate(true).use_built_in_libdl(true);

    // Add base libraries (order matters for symbol resolution)
    linker = linker
        .library("libwasi-emulated-process-clocks.so", wasi_clocks, false)
        .map_err(|e| JsError::new(&format!("libwasi-emulated-process-clocks.so: {e}")))?
        .library("libwasi-emulated-signal.so", wasi_signal, false)
        .map_err(|e| JsError::new(&format!("libwasi-emulated-signal.so: {e}")))?
        .library("libwasi-emulated-mman.so", wasi_mman, false)
        .map_err(|e| JsError::new(&format!("libwasi-emulated-mman.so: {e}")))?
        .library("libwasi-emulated-getpid.so", wasi_getpid, false)
        .map_err(|e| JsError::new(&format!("libwasi-emulated-getpid.so: {e}")))?
        // C/C++ runtime
        .library("libc.so", libc, false)
        .map_err(|e| JsError::new(&format!("libc.so: {e}")))?
        .library("libc++abi.so", libcxxabi, false)
        .map_err(|e| JsError::new(&format!("libc++abi.so: {e}")))?
        .library("libc++.so", libcxx, false)
        .map_err(|e| JsError::new(&format!("libc++.so: {e}")))?
        // Python
        .library("libpython3.14.so", libpython, false)
        .map_err(|e| JsError::new(&format!("libpython3.14.so: {e}")))?
        // Our runtime and bindings
        .library("liberyx_runtime.so", runtime, false)
        .map_err(|e| JsError::new(&format!("liberyx_runtime.so: {e}")))?
        .library("liberyx_bindings.so", bindings, false)
        .map_err(|e| JsError::new(&format!("liberyx_bindings.so: {e}")))?;

    // Add user's native extensions (dl_openable = true for dlopen/dlsym)
    for ext in &extensions {
        linker = linker
            .library(&ext.name, &ext.bytes, true)
            .map_err(|e| JsError::new(&format!("{}: {e}", ext.name)))?;
    }

    // Add WASI adapter
    linker = linker
        .adapter("wasi_snapshot_preview1", adapter)
        .map_err(|e| JsError::new(&format!("WASI adapter: {e}")))?;

    linker
        .encode()
        .map_err(|e| JsError::new(&format!("encoding: {e}")))
}
