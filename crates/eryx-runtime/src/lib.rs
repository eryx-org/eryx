//! Eryx Python WASM Runtime
//!
//! This crate contains the WIT definition and Python source for the eryx
//! sandbox runtime. The actual WASM component is built using `componentize-py`.
//!
//! ## Contents
//!
//! - `runtime.wit` - WIT interface definition
//! - `runtime.py` - Python runtime implementation
//!
//! ## Building the WASM Component
//!
//! ```bash
//! cd crates/eryx-runtime
//!
//! # Generate bindings
//! componentize-py -d runtime.wit -w sandbox bindings guest_bindings
//!
//! # Build the component
//! componentize-py -d runtime.wit -w sandbox componentize runtime -o runtime.wasm
//! ```
//!
//! See the crate README for more details.

/// The WIT definition as a string constant.
///
/// This can be used for documentation or tooling purposes.
pub const WIT_DEFINITION: &str = include_str!("../runtime.wit");

/// The Python runtime source as a string constant.
///
/// This can be used for documentation or tooling purposes.
pub const PYTHON_RUNTIME: &str = include_str!("../runtime.py");
