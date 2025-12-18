//! Eryx Python WASM Runtime
//!
//! This crate contains the WIT definition and builds the eryx sandbox WASM component.
//! The component uses our custom eryx-wasm-runtime (liberyx_runtime.so) for Python
//! execution via CPython FFI.
//!
//! ## Features
//!
//! - `native-extensions` - Enable native Python extension support via late-linking
//!   and pre-initialization. This allows adding extensions like numpy at sandbox
//!   creation time without rebuilding the entire component, and captures Python's
//!   memory state for faster startup.
//!
//! ## Contents
//!
//! - `runtime.wit` - WIT interface definition
//! - `linker` - Late-linking support for native extensions (feature-gated)
//! - `preinit` - Pre-initialization support (feature-gated)
//!
//! ## See Also
//!
//! - `eryx-wasm-runtime` - The custom runtime that implements the WIT exports

/// The WIT definition as a string constant.
pub const WIT_DEFINITION: &str = include_str!("../runtime.wit");

/// Late-linking support for native Python extensions.
#[cfg(feature = "native-extensions")]
pub mod linker;

/// Pre-initialization support for capturing Python memory state.
#[cfg(feature = "native-extensions")]
pub mod preinit;

/// Stub WASI adapters for pre-initialization.
#[cfg(feature = "native-extensions")]
pub mod stubwasi;
