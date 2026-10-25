//! The base libraries that make up the runtime component, and their link order.
//!
//! `linker.rs` uses this module directly, and `build.rs` and `eryx-linker-wasm`
//! include the file via `#[path]`, so the three linkers cannot drift apart. It
//! must not depend on anything but `core`.

/// The base libraries, by role.
pub(crate) struct BaseLibraries<'a> {
    pub(crate) wasi_clocks: &'a [u8],
    pub(crate) wasi_signal: &'a [u8],
    pub(crate) wasi_mman: &'a [u8],
    pub(crate) wasi_getpid: &'a [u8],
    pub(crate) libc: &'a [u8],
    pub(crate) libcxxabi: &'a [u8],
    pub(crate) libcxx: &'a [u8],
    pub(crate) libpython: &'a [u8],
    pub(crate) runtime: &'a [u8],
    pub(crate) bindings: &'a [u8],
}

impl<'a> BaseLibraries<'a> {
    /// The libraries as `(soname, bytes)` pairs in link order: every library
    /// comes after the libraries it depends on.
    pub(crate) fn in_link_order(&self) -> [(&'static str, &'a [u8]); 10] {
        [
            // WASI emulation libs
            ("libwasi-emulated-process-clocks.so", self.wasi_clocks),
            ("libwasi-emulated-signal.so", self.wasi_signal),
            ("libwasi-emulated-mman.so", self.wasi_mman),
            ("libwasi-emulated-getpid.so", self.wasi_getpid),
            // C/C++ runtime
            ("libc.so", self.libc),
            ("libc++abi.so", self.libcxxabi),
            ("libc++.so", self.libcxx),
            // Python
            ("libpython3.14.so", self.libpython),
            // Our runtime and bindings
            ("liberyx_runtime.so", self.runtime),
            ("liberyx_bindings.so", self.bindings),
        ]
    }
}
