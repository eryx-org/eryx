//! Eryx Python WASM Runtime
//!
//! This crate contains the WIT definition and builds the eryx sandbox WASM component.
//! The component uses our custom eryx-wasm-runtime (liberyx_runtime.so) for Python
//! execution via CPython FFI.
//!
//! ## Features
//!
//! - `preinit` - Pre-initialization support for capturing Python memory state.
//!   Provides ~25x speedup for sandbox creation. Works with or without native
//!   extensions - can pre-import stdlib modules only.
//!
//! - `native-extensions` - Native Python extension support via late-linking.
//!   Allows adding extensions like numpy at sandbox creation time. Implies `preinit`.
//!
//! ## Contents
//!
//! - `runtime.wit` - WIT interface definition
//! - `linker` - Late-linking support for native extensions (requires `preinit`)
//! - `preinit` - Pre-initialization support (requires `preinit` feature)
//! - `stubwasi` - Stub WASI adapters for pre-initialization (requires `preinit`)
//!
//! ## See Also
//!
//! - `eryx-wasm-runtime` - The custom runtime that implements the WIT exports

/// The WIT definition as a string constant.
pub const WIT_DEFINITION: &str = include_str!("../runtime.wit");

/// Late-linking support for native Python extensions.
#[cfg(feature = "preinit")]
pub mod linker;

/// Pre-initialization support for capturing Python memory state.
#[cfg(feature = "preinit")]
pub mod preinit;

/// Stub WASI adapters for pre-initialization.
#[cfg(feature = "preinit")]
pub mod stubwasi;
