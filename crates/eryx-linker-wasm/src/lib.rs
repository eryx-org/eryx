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

#[path = "../../eryx-runtime/src/link_order.rs"]
mod link_order;

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
/// decompression via fzstd). They are linked in the same order as
/// `eryx-runtime`'s linker, from the shared `link_order.rs`.
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
    // wit-component 0.257 turned `Linker` into a `&mut self` builder and moved
    // `validate`/`adapter` onto the inner `ComponentEncoder`.
    let mut linker = Linker::default();
    linker.use_built_in_libdl(true);
    linker.encoder().validate(true);

    let libs = link_order::BaseLibraries {
        wasi_clocks,
        wasi_signal,
        wasi_mman,
        wasi_getpid,
        libc,
        libcxxabi,
        libcxx,
        libpython,
        runtime,
        bindings,
    };
    for (name, bytes) in libs.in_link_order() {
        linker
            .library(name, bytes, false)
            .map_err(|e| JsError::new(&format!("{name}: {e}")))?;
    }

    // Add user's native extensions (dl_openable = true for dlopen/dlsym)
    for ext in &extensions {
        linker
            .library(&ext.name, &ext.bytes, true)
            .map_err(|e| JsError::new(&format!("{}: {e}", ext.name)))?;
    }

    // Add WASI adapter
    linker
        .encoder()
        .adapter("wasi_snapshot_preview1", adapter)
        .map_err(|e| JsError::new(&format!("WASI adapter: {e}")))?;

    linker
        .encode()
        .map_err(|e| JsError::new(&format!("encoding: {e}")))
}
