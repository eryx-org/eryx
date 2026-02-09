//! # Eryx
//!
//! A Python sandbox with async callbacks powered by WebAssembly.
//!
//! ## Safety
//!
//! By default, this crate uses `#![forbid(unsafe_code)]` for maximum safety.
//! When the `embedded` or `preinit` features are enabled, this is relaxed to
//! `#![deny(unsafe_code)]` to allow unsafe wasmtime APIs needed for pre-compiled
//! component loading and CPU feature configuration (e.g., disabling AVX-512).
//!
//! Eryx executes Python code in a secure WebAssembly sandbox with:
//!
//! - **Async callback mechanism** - Callbacks are exposed as direct async functions (e.g., `await get_time()`)
//! - **Parallel execution** - Multiple callbacks can run concurrently via `asyncio.gather()`
//! - **Execution tracing** - Line-level progress reporting via `sys.settrace`
//! - **Introspection** - Python can discover available callbacks at runtime
//! - **Composable runtime libraries** - Pre-built APIs with Python wrappers and type stubs
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use eryx::Sandbox;
//!
//! # #[cfg(feature = "embedded")]
//! #[tokio::main]
//! async fn main() -> Result<(), eryx::Error> {
//!     // Use Sandbox::embedded() for zero-config setup (requires `embedded` feature)
//!     let sandbox = Sandbox::embedded().build()?;
//!
//!     let result = sandbox.execute("print('Hello from Python!')").await?;
//!
//!     println!("Output: {}", result.stdout);
//!     Ok(())
//! }
//! # #[cfg(not(feature = "embedded"))]
//! # fn main() {}
//! ```

// Safety lint configuration:
// - Default: forbid unsafe code entirely
// - With `embedded` feature: deny unsafe code, but allow it on specific items
//   that need wasmtime's unsafe deserialization APIs
// - With `preinit` feature: deny unsafe code, needed for cranelift CPU feature flags
//   during precompilation (e.g., disabling AVX-512 for portable builds)
#![cfg_attr(
    not(any(feature = "embedded", feature = "preinit")),
    forbid(unsafe_code)
)]
#![cfg_attr(any(feature = "embedded", feature = "preinit"), deny(unsafe_code))]

pub mod cache;
mod callback;
pub mod callback_handler;
#[cfg(feature = "embedded")]
pub mod embedded;
mod error;
mod library;
pub mod net;
pub mod package;
pub mod pool;
mod sandbox;
mod schema;
pub mod secrets;
pub mod session;
mod trace;
mod wasm;

/// Pre-initialization support for capturing Python memory state.
///
/// Pre-initialization runs Python's init + imports during build time and
/// captures the memory state into the component. This provides ~25x speedup
/// by avoiding the ~450ms startup cost on each sandbox creation.
///
/// Works with or without native extensions - can pre-import stdlib modules only.
#[cfg(feature = "preinit")]
pub mod preinit {
    pub use eryx_runtime::linker::NativeExtension;
    pub use eryx_runtime::preinit::{PreInitError, pre_initialize};
}

pub use callback::{
    Callback, CallbackError, DynamicCallback, DynamicCallbackBuilder, TypedCallback, empty_schema,
};
pub use error::Error;
pub use library::RuntimeLibrary;
pub use net::{ConnectionManager, NetConfig, TcpError, TlsError};
pub use package::{ExtractedPackage, PackageFormat};
pub use pool::{PoolConfig, PoolError, PoolStats, PooledSandbox, SandboxPool};
pub use sandbox::{
    ExecuteResult, ExecuteStats, ExecutionHandle, ResourceLimits, Sandbox, SandboxBuilder, state,
};
pub use secrets::{FileScrubPolicy, OutputScrubPolicy, SecretConfig};
pub use session::{
    InProcessSession, PythonStateSnapshot, Session, SessionExecutor, SnapshotMetadata,
    SnapshotSession,
};
#[cfg(feature = "vfs")]
pub use session::{VfsConfig, VolumeMount};
pub use trace::{OutputHandler, TraceEvent, TraceEventKind, TraceHandler};

// Re-export precompilation utilities and internal types
pub use wasm::{CpuFeatureLevel, ExecutionOutput, NetRequest, OutputRequest, PythonExecutor};

// Re-export schema types at top level for convenience
pub use schema::{JsonSchema, Schema};

// Re-export CancellationToken for convenient use with execute_cancellable
pub use tokio_util::sync::CancellationToken;

// Re-export the callback proc macro when the macros feature is enabled
#[cfg(feature = "macros")]
pub use eryx_macros::callback;

/// Virtual filesystem support for sandboxed file operations.
///
/// When the `vfs` feature is enabled, sandboxes can use an in-memory filesystem
/// that persists across executions. This is useful for:
///
/// - Allowing Python code to read/write files without host filesystem access
/// - Persisting files across multiple executions
/// - Testing file operations in isolation
///
/// Virtual filesystem types for sandboxed file I/O.
///
/// VFS is available on `SessionExecutor` for persistent file storage across
/// multiple executions within a session.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::vfs::InMemoryStorage;
/// use std::sync::Arc;
///
/// let storage = Arc::new(InMemoryStorage::new());
/// let session = SessionExecutor::new_with_vfs(executor, &[], storage).await?;
///
/// session.execute("open('/data/test.txt', 'w').write('hello')").run().await?;
/// session.execute("print(open('/data/test.txt').read())").run().await?;  // prints "hello"
/// ```
#[cfg(feature = "vfs")]
pub mod vfs {
    pub use eryx_vfs::{
        DirEntry, DirPerms, FilePerms, InMemoryStorage, Metadata, ScrubbingStorage, VfsCtx,
        VfsError, VfsFileScrubPolicy, VfsResult, VfsSecretConfig, VfsState, VfsStorage, VfsView,
    };
}
