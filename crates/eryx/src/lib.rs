//! # Eryx
//!
//! A Python sandbox with async callbacks powered by WebAssembly.
//!
//! ## Safety
//!
//! By default, this crate uses `#![forbid(unsafe_code)]` for maximum safety.
//! When the `precompiled` feature is enabled, this is relaxed to `#![deny(unsafe_code)]`
//! to allow the unsafe wasmtime deserialization APIs needed for pre-compiled components.
//!
//! Eryx executes Python code in a secure WebAssembly sandbox with:
//!
//! - **Async callback mechanism** - Python can `await invoke("callback_name", ...)` to call host-provided functions
//! - **Parallel execution** - Multiple callbacks can run concurrently via `asyncio.gather()`
//! - **Execution tracing** - Line-level progress reporting via `sys.settrace`
//! - **Introspection** - Python can discover available callbacks at runtime
//! - **Composable runtime libraries** - Pre-built APIs with Python wrappers and type stubs
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use eryx::Sandbox;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), eryx::Error> {
//!     let sandbox = Sandbox::builder().build()?;
//!
//!     let result = sandbox.execute(r#"
//!         print("Hello from Python!")
//!     "#).await?;
//!
//!     println!("Output: {}", result.stdout);
//!     Ok(())
//! }
//! ```

// Safety lint configuration:
// - Default: forbid unsafe code entirely
// - With `precompiled` feature: deny unsafe code, but allow it on specific items
//   that need wasmtime's unsafe deserialization APIs
#![cfg_attr(not(feature = "precompiled"), forbid(unsafe_code))]
#![cfg_attr(feature = "precompiled", deny(unsafe_code))]

mod callback;
mod error;
mod library;
mod sandbox;
pub mod session;
mod trace;
mod wasm;

pub use callback::{Callback, CallbackError};
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
