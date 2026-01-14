//! In-process session: keeps WASM instance alive between executions.
//!
//! This module provides `InProcessSession`, a high-level session API that wraps
//! `SessionExecutor` to provide state persistence between `execute()` calls.
//!
//! ## How It Works
//!
//! `InProcessSession` delegates to `SessionExecutor` internally, which keeps the
//! WASM Store and Instance alive between executions. The Python runtime maintains
//! a `_persistent_globals` dict that preserves user-defined variables.
//!
//! ## Trade-offs
//!
//! **Pros:**
//! - Fastest approach: no instance recreation overhead
//! - No ~15ms WASM instantiation overhead after first call
//! - State persists: variables, functions, classes available across calls
//! - Simple high-level API
//!
//! **Cons:**
//! - State cannot be persisted across process restarts (use `save()` for that)
//! - Memory stays allocated until session is dropped
//!
//! ## Example
//!
//! ```rust,ignore
//! use eryx::session::{InProcessSession, Session};
//!
//! let sandbox = Sandbox::builder()
//!     .with_embedded_runtime()
//!     .build()?;
//!
//! let mut session = InProcessSession::new(&sandbox).await?;
//!
//! // State persists between calls!
//! session.execute("x = 1").await?;
//! session.execute("y = 2").await?;
//! let result = session.execute("print(x + y)").await?;
//! assert_eq!(result.stdout, "3");
//!
//! // Save to disk for later
//! session.save("my_session.session").await?;
//!
//! // Reset clears all state
//! session.reset().await?;
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::callback::Callback;
use crate::callback_handler::{run_callback_handler, run_trace_collector};
use crate::error::Error;
use crate::sandbox::{ExecuteResult, ExecuteStats, Sandbox};
use crate::wasm::{CallbackRequest, TraceRequest};

use super::Session;
use super::executor::{PythonStateSnapshot, SessionExecutor};
use super::persistence::PersistedSession;

/// An in-process session that keeps the WASM instance alive between executions.
///
/// This provides the fastest session performance by avoiding instance creation
/// overhead and maintaining Python state between calls.
///
/// Internally delegates to [`SessionExecutor`] for WASM instance management.
pub struct InProcessSession<'a> {
    /// Reference to the parent sandbox for configuration.
    sandbox: &'a Sandbox,

    /// The underlying session executor that manages the WASM instance.
    executor: SessionExecutor,

    /// Whether the preamble has been executed.
    preamble_executed: bool,
}

impl std::fmt::Debug for InProcessSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessSession")
            .field("execution_count", &self.executor.execution_count())
            .field("preamble_executed", &self.preamble_executed)
            .finish_non_exhaustive()
    }
}

impl<'a> InProcessSession<'a> {
    /// Create a new in-process session from a sandbox.
    ///
    /// The session will share the sandbox's configuration (callbacks, preamble, etc.)
    /// but maintain its own persistent state.
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be initialized.
    pub async fn new(sandbox: &'a Sandbox) -> Result<Self, Error> {
        let callbacks: Vec<Arc<dyn Callback>> = sandbox.callbacks().values().cloned().collect();

        let mut executor = SessionExecutor::new(sandbox.executor().clone(), &callbacks).await?;

        // Set execution timeout from sandbox resource limits
        executor.set_execution_timeout(sandbox.resource_limits().execution_timeout);

        Ok(Self {
            sandbox,
            executor,
            preamble_executed: false,
        })
    }

    /// Execute Python code, maintaining state between calls.
    ///
    /// Variables, functions, and classes defined in one call are available
    /// in subsequent calls. For example:
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").await?;
    /// session.execute("print(x)").await?;  // prints "1"
    /// ```
    async fn execute_internal(&mut self, code: &str) -> Result<ExecuteResult, Error> {
        let start = Instant::now();

        // Execute preamble on first call if configured
        let full_code = if !self.preamble_executed && !self.sandbox.preamble().is_empty() {
            self.preamble_executed = true;
            format!("{}\n\n# User code\n{}", self.sandbox.preamble(), code)
        } else {
            code.to_string()
        };

        // Create channels for callback requests and trace events
        let (callback_tx, callback_rx) = mpsc::channel::<CallbackRequest>(32);
        let (trace_tx, trace_rx) = mpsc::unbounded_channel::<TraceRequest>();

        // Spawn task to handle callback requests concurrently (Arc clone is cheap)
        let callbacks_arc = self.sandbox.callbacks_arc();
        let resource_limits = self.sandbox.resource_limits().clone();
        let callback_handler = tokio::spawn(async move {
            run_callback_handler(callback_rx, callbacks_arc, resource_limits).await
        });

        // Spawn task to handle trace events
        let trace_handler = self.sandbox.trace_handler().clone();
        let trace_collector =
            tokio::spawn(async move { run_trace_collector(trace_rx, trace_handler).await });

        // Get callbacks for this execution
        let callbacks: Vec<Arc<dyn Callback>> =
            self.sandbox.callbacks().values().cloned().collect();

        // Execute using the session executor (keeps instance alive!)
        // Timeout is handled via epoch-based interruption inside the executor
        let execution_result = self
            .executor
            .execute(&full_code)
            .with_callbacks(&callbacks, callback_tx)
            .with_tracing(trace_tx)
            .run()
            .await;

        // Wait for the handler tasks to complete
        let callback_invocations = callback_handler.await.unwrap_or(0);
        let trace_events = trace_collector.await.unwrap_or_default();

        let duration = start.elapsed();

        match execution_result {
            Ok(output) => {
                // Stream output if handler is configured
                if let Some(handler) = self.sandbox.output_handler() {
                    handler.on_output(&output.stdout).await;
                }

                Ok(ExecuteResult {
                    stdout: output.stdout,
                    trace: trace_events,
                    stats: ExecuteStats {
                        duration,
                        callback_invocations,
                        peak_memory_bytes: Some(output.peak_memory_bytes),
                    },
                })
            }
            Err(error) => Err(Error::Execution(error)),
        }
    }

    /// Get the number of executions performed in this session.
    #[must_use]
    pub fn execution_count(&self) -> u32 {
        self.executor.execution_count()
    }

    /// Capture a snapshot of the current Python session state.
    ///
    /// See [`SessionExecutor::snapshot_state`] for details.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be captured.
    pub async fn snapshot_state(&mut self) -> Result<PythonStateSnapshot, Error> {
        self.executor.snapshot_state().await
    }

    /// Restore Python session state from a previously captured snapshot.
    ///
    /// See [`SessionExecutor::restore_state`] for details.
    ///
    /// # Errors
    ///
    /// Returns an error if the restore fails.
    pub async fn restore_state(&mut self, snapshot: &PythonStateSnapshot) -> Result<(), Error> {
        self.executor.restore_state(snapshot).await
    }

    /// Clear all persistent state from the session.
    ///
    /// This is lighter-weight than `reset()` because it doesn't recreate
    /// the WASM instance - it just clears the Python-level state.
    ///
    /// # Errors
    ///
    /// Returns an error if the clear fails.
    pub async fn clear_state(&mut self) -> Result<(), Error> {
        self.executor.clear_state().await
    }

    // =========================================================================
    // Persistence Methods
    // =========================================================================

    /// Save the session state to disk.
    ///
    /// This captures the current Python state (variables, functions, classes)
    /// and writes it to a file that can be loaded later with [`load`](Self::load).
    ///
    /// # Arguments
    ///
    /// * `path` - The path to save the session to (should end in `.session`)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The state cannot be captured
    /// - The file cannot be written
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 42").await?;
    /// session.save("my_session.session").await?;
    /// ```
    pub async fn save(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        // Capture the current state
        let snapshot = self.snapshot_state().await?;

        // Create the persisted session
        let persisted = PersistedSession::new(
            snapshot.data().to_vec(),
            self.execution_count(),
            None, // New save, use current time as created_at
        );

        // Save to disk
        persisted.save(path).await
    }

    /// Save the session state to disk, preserving the original creation time.
    ///
    /// This is useful when re-saving a session that was previously loaded,
    /// to preserve the original creation timestamp.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to save the session to
    /// * `created_at` - The original creation time to preserve
    ///
    /// # Errors
    ///
    /// Returns an error if the state cannot be captured or the file cannot be written.
    pub async fn save_with_created_at(
        &mut self,
        path: impl AsRef<Path>,
        created_at: SystemTime,
    ) -> Result<(), Error> {
        let snapshot = self.snapshot_state().await?;

        let persisted = PersistedSession::new(
            snapshot.data().to_vec(),
            self.execution_count(),
            Some(created_at),
        );

        persisted.save(path).await
    }

    /// Load a session from disk.
    ///
    /// This creates a new session and restores the Python state from a
    /// previously saved session file.
    ///
    /// # Arguments
    ///
    /// * `sandbox` - The sandbox to create the session in
    /// * `path` - The path to load the session from
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be read
    /// - The session format is incompatible
    /// - The state cannot be restored
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut session = InProcessSession::load(&sandbox, "my_session.session").await?;
    /// let result = session.execute("print(x)").await?;  // prints "42"
    /// ```
    pub async fn load(sandbox: &'a Sandbox, path: impl AsRef<Path>) -> Result<Self, Error> {
        // Load the persisted session from disk
        let persisted = PersistedSession::load(path).await?;

        // Create a new session
        let mut session = Self::new(sandbox).await?;

        // Restore the state using the internal snapshot format
        // The persisted state is raw pickle bytes, we need to wrap it
        let snapshot = PythonStateSnapshot::new(persisted.state);
        session.restore_state(&snapshot).await?;

        Ok(session)
    }
}

#[async_trait]
impl Session for InProcessSession<'_> {
    async fn execute(&mut self, code: &str) -> Result<ExecuteResult, Error> {
        self.execute_internal(code).await
    }

    async fn reset(&mut self) -> Result<(), Error> {
        // Reset the underlying executor
        let callbacks: Vec<Arc<dyn Callback>> =
            self.sandbox.callbacks().values().cloned().collect();
        self.executor.reset(&callbacks).await?;

        // Reset preamble flag so it runs again on next execute
        self.preamble_executed = false;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_process_session_size() {
        // Basic struct test - verify the struct has expected fields
        // The size will vary based on the SessionExecutor internals
        assert!(std::mem::size_of::<InProcessSession<'_>>() > 0);
    }
}
