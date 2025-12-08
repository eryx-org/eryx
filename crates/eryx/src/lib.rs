//! # Eryx
//!
//! A Python sandbox with async callbacks powered by WebAssembly.
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

mod callback;
mod error;
mod library;
mod sandbox;
mod trace;
mod wasm;

pub use callback::{Callback, CallbackError};
pub use error::Error;
pub use library::RuntimeLibrary;
pub use sandbox::{ExecuteResult, ExecuteStats, ResourceLimits, Sandbox, SandboxBuilder};
pub use trace::{OutputHandler, TraceEvent, TraceEventKind, TraceHandler};
