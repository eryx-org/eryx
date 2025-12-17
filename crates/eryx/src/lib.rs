//! # Eryx
//!
//! A Python sandbox with async callbacks powered by WebAssembly.
//!
//! ## Safety
//!
//! By default, this crate uses `#![forbid(unsafe_code)]` for maximum safety.
//! When the `embedded` feature is enabled, this is relaxed to `#![deny(unsafe_code)]`
//! to allow the unsafe wasmtime deserialization APIs needed for pre-compiled components.
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
//! ```rust
//! use eryx::Sandbox;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), eryx::Error> {
//!     let sandbox = Sandbox::builder().build()?;
//!
//!     let result = sandbox.execute("print('Hello from Python!')").await?;
//!
//!     println!("Output: {}", result.stdout);
//!     Ok(())
//! }
//! ```

// Safety lint configuration:
// - Default: forbid unsafe code entirely
// - With `embedded` feature: deny unsafe code, but allow it on specific items
//   that need wasmtime's unsafe deserialization APIs
#![cfg_attr(not(feature = "embedded"), forbid(unsafe_code))]
#![cfg_attr(feature = "embedded", deny(unsafe_code))]

pub mod cache;
mod callback;
#[cfg(feature = "embedded")]
pub mod embedded;
mod error;
mod library;
pub mod package;
mod sandbox;
mod schema;
pub mod session;
mod trace;
mod wasm;

/// Pre-initialization support for native extensions.
///
/// Pre-initialization runs Python's init + imports during build time and
/// captures the memory state into the component. This avoids the 50-100ms
/// startup cost on each sandbox creation.
#[cfg(feature = "native-extensions")]
pub mod preinit {
    pub use eryx_runtime::preinit::{PreInitError, pre_initialize};
}

pub use callback::{
    Callback, CallbackError, DynamicCallback, DynamicCallbackBuilder, TypedCallback, empty_schema,
};
pub use error::Error;
pub use library::RuntimeLibrary;
pub use sandbox::{ExecuteResult, ExecuteStats, ResourceLimits, Sandbox, SandboxBuilder};
pub use session::{
    InProcessSession, PythonStateSnapshot, Session, SessionExecutor, SnapshotMetadata,
    SnapshotSession,
};
pub use trace::{OutputHandler, TraceEvent, TraceEventKind, TraceHandler};

// Re-export precompilation utilities and internal types
pub use wasm::{ExecutionOutput, PythonExecutor};

// Re-export schema types at top level for convenience
pub use schema::{JsonSchema, Schema};
