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

use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::error::Error;
use crate::sandbox::ExecuteResult;

pub use executor::{PythonStateSnapshot, SessionExecutor, SnapshotMetadata};
#[cfg(feature = "vfs")]
pub use executor::{VfsConfig, VolumeMount};
pub use in_process::InProcessSession;

/// Statistics about a session's activity and resource usage.
///
/// This struct tracks various metrics about a session's lifetime, including
/// when it was created, when it was last active, and aggregate execution statistics.
///
/// # Example
///
/// ```rust,ignore
/// let mut session = InProcessSession::new(&sandbox).await?;
/// session.execute("x = 1").await?;
/// session.execute("y = 2").await?;
///
/// let stats = session.stats();
/// println!("Executions: {}", stats.execution_count);
/// println!("Total time: {:?}", stats.total_execution_time);
/// println!("Idle for: {:?}", session.idle_duration());
/// ```
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// When the session was created.
    pub created_at: Instant,

    /// When the last execution completed (None if never executed).
    pub last_activity: Option<Instant>,

    /// Total number of executions performed.
    pub execution_count: u64,

    /// Total time spent executing code across all runs.
    pub total_execution_time: Duration,

    /// Total number of callback invocations across all executions.
    pub total_callback_invocations: u64,

    /// Peak memory usage observed across all executions (in bytes).
    pub peak_memory_bytes: u64,
}

impl Default for SessionStats {
    fn default() -> Self {
        Self {
            created_at: Instant::now(),
            last_activity: None,
            execution_count: 0,
            total_execution_time: Duration::ZERO,
            total_callback_invocations: 0,
            peak_memory_bytes: 0,
        }
    }
}

impl SessionStats {
    /// Create a new SessionStats with the current time as creation time.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all statistics to their default values, preserving the original creation time.
    pub fn reset(&mut self) {
        let created_at = self.created_at;
        *self = Self::default();
        self.created_at = created_at;
    }

    /// Reset all statistics to their default values and update the creation time.
    pub fn reset_full(&mut self) {
        *self = Self::default();
    }
}

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

    #[test]
    fn test_session_stats_default() {
        let stats = SessionStats::default();
        assert_eq!(stats.execution_count, 0);
        assert_eq!(stats.total_execution_time, Duration::ZERO);
        assert_eq!(stats.total_callback_invocations, 0);
        assert_eq!(stats.peak_memory_bytes, 0);
        assert!(stats.last_activity.is_none());
    }

    #[test]
    fn test_session_stats_reset() {
        let mut stats = SessionStats::default();
        let original_created_at = stats.created_at;

        // Modify stats
        stats.execution_count = 10;
        stats.total_execution_time = Duration::from_secs(5);
        stats.total_callback_invocations = 20;
        stats.peak_memory_bytes = 1024;
        stats.last_activity = Some(Instant::now());

        // Reset preserving created_at
        stats.reset();

        assert_eq!(stats.created_at, original_created_at);
        assert_eq!(stats.execution_count, 0);
        assert_eq!(stats.total_execution_time, Duration::ZERO);
        assert_eq!(stats.total_callback_invocations, 0);
        assert_eq!(stats.peak_memory_bytes, 0);
        assert!(stats.last_activity.is_none());
    }

    #[test]
    fn test_session_stats_reset_full() {
        let mut stats = SessionStats::default();
        let original_created_at = stats.created_at;

        // Small delay to ensure new created_at differs
        std::thread::sleep(Duration::from_millis(1));

        // Full reset (updates created_at)
        stats.reset_full();

        // created_at should be updated (newer)
        assert!(stats.created_at >= original_created_at);
        assert_eq!(stats.execution_count, 0);
    }
}
