//! Virtual Filesystem for Eryx Sandbox
//!
//! Provides a custom `wasi:filesystem` implementation backed by a key-value
//! store, allowing sandboxed Python code to read and write files that persist
//! across sandbox executions.
//!
//! ## Architecture
//!
//! The VFS consists of:
//! - [`VfsStorage`] - A trait for pluggable storage backends
//! - [`InMemoryStorage`] - An in-memory implementation for testing
//! - WASI host implementations that bridge storage to the component model
//!
//! ## Usage
//!
//! ```rust,ignore
//! use eryx_vfs::{InMemoryStorage, VfsCtx, VfsState, VfsView, add_vfs_to_linker};
//! use std::sync::Arc;
//!
//! // Create storage
//! let storage = Arc::new(InMemoryStorage::new());
//!
//! // Create VFS context
//! let mut vfs_ctx = VfsCtx::new(storage);
//!
//! // Add WASI to linker first, then override filesystem with VFS
//! wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
//! add_vfs_to_linker(&mut linker)?;
//! ```

#![deny(unsafe_code)]

mod bindings;
mod error;
mod host;
pub mod hybrid;
mod hybrid_bindings;
mod hybrid_host;
mod linker;
pub mod scrubbing;
mod storage;
mod streams;
mod wasi_impl;

pub use error::{VfsError, VfsResult};
pub use hybrid::{
    HybridDescriptor, HybridPreopen, HybridVfsCtx, HybridVfsState, RealDir, RealFile, RestrictedDir,
};
pub use hybrid_bindings::HybridReaddirIterator;
pub use linker::{HybridVfsView, VfsView, add_hybrid_vfs_to_linker, add_vfs_to_linker};
pub use scrubbing::{
    FileScrubPolicy as VfsFileScrubPolicy, ScrubbingStorage, SecretConfig as VfsSecretConfig,
};
pub use storage::{DirEntry, InMemoryStorage, Metadata, VfsStorage};
pub use wasi_impl::{VfsCtx, VfsDescriptor, VfsReaddirIterator, VfsState};

// Re-export permission types from wasmtime-wasi for convenience
pub use wasmtime_wasi::{DirPerms, FilePerms};
