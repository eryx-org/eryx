//! Session-based execution for persistent WASM state.
//!
//! This module provides session-based execution that maintains state between
//! WASM executions, avoiding the ~1.5ms Python interpreter initialization
//! overhead on each call.
//!
//! ## Available Session Types
//!
//! - **[`SessionExecutor`]**: Core executor that keeps WASM Store and Instance alive
//!   between executions. This is the foundation for session-based execution.
//!
//! - **[`InProcessSession`]**: High-level session API wrapping the sandbox.
//!   Provides a simple interface for REPL-style interactive execution.
//!
//! ## State Persistence (Coming Soon)
//!
//! The WIT export approach will enable Python-level state snapshots via
//! `snapshot_state()` and `restore_state()` exports in the runtime. This
//! provides serializable state with minimal overhead (~KB vs ~50MB for
//! WASM-level snapshots).
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::session::{InProcessSession, Session};
//!
//! // Create a sandbox and start a session
//! let sandbox = Sandbox::builder()
//!     .with_embedded_runtime()
//!     .build()?;
//!
//! let mut session = InProcessSession::new(&sandbox).await?;
//!
//! // Execute multiple statements, preserving state
//! session.execute("x = 1").await?;
//! session.execute("y = 2").await?;
//! let result = session.execute("print(x + y)").await?;
//! assert_eq!(result.stdout, "3");
//!
//! // Reset to fresh state
//! session.reset().await?;
//! ```

pub mod executor;
pub mod in_process;

use async_trait::async_trait;

use crate::error::Error;
use crate::sandbox::ExecuteResult;

pub use executor::{PythonStateSnapshot, SessionExecutor, SnapshotMetadata};
pub use in_process::InProcessSession;

/// Common trait for all session implementations.
///
/// A session maintains persistent state across multiple `execute()` calls,
/// avoiding the ~1.5ms Python interpreter initialization overhead on each call.
#[async_trait]
pub trait Session: Send {
    /// Execute Python code within this session.
    ///
    /// State from previous executions is preserved. For example:
    /// - `execute("x = 1")` followed by `execute("print(x)")` will print "1"
    ///
    /// # Errors
    ///
    /// Returns an error if the Python code fails to execute or a resource limit is exceeded.
    async fn execute(&mut self, code: &str) -> Result<ExecuteResult, Error>;

    /// Reset the session to a fresh state.
    ///
    /// After reset, previously defined variables will no longer be accessible.
    ///
    /// # Errors
    ///
    /// Returns an error if the reset fails.
    async fn reset(&mut self) -> Result<(), Error>;
}

/// Trait for sessions that support state snapshots.
///
/// Snapshots capture the current state of the session so it can be:
/// - Persisted to disk or a database
/// - Sent over the network to another process
/// - Restored later to continue execution
///
/// # Snapshot Timing
///
/// Snapshots can only be captured when `execute()` has returned. It is not
/// possible to snapshot mid-execution (e.g., while Python code is running).
/// This is a fundamental limitation of JIT-compiled WASM.
///
/// # Implementation Note
///
/// The recommended approach for snapshots is the WIT export method, where
/// Python-level state is serialized via `pickle` and exposed through
/// `snapshot_state()` and `restore_state()` exports in the runtime.
pub trait SnapshotSession: Session {
    /// The type of snapshot produced by this session.
    type Snapshot: Send + Sync;

    /// Capture a snapshot of the current session state.
    ///
    /// The snapshot can later be restored using [`SnapshotSession::restore`] or
    /// used to create a new session with the captured state.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be captured.
    fn snapshot(&self) -> Result<Self::Snapshot, Error>;

    /// Restore session state from a snapshot.
    ///
    /// This replaces the current session state with the state from the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot is invalid or incompatible with this session.
    fn restore(&mut self, snapshot: &Self::Snapshot) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_trait_is_object_safe() {
        // Verify the Session trait can be used as a trait object
        fn _assert_object_safe(_: &dyn Session) {}
        fn _assert_boxed(_: Box<dyn Session>) {}
    }

    #[test]
    fn test_snapshot_session_trait_exists() {
        // Verify SnapshotSession trait is properly defined
        fn _assert_snapshot_session<T: SnapshotSession>() {}
    }
}
