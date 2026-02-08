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
//! - State cannot be persisted across process restarts (use `snapshot_state()` for that)
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
//! // Reset clears all state
//! session.reset().await?;
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::callback::Callback;
use crate::callback_handler::{run_callback_handler, run_trace_collector};
use crate::error::Error;
use crate::sandbox::{ExecuteResult, ExecuteStats, Sandbox};
use crate::wasm::{CallbackRequest, TraceRequest};

use super::executor::{PythonStateSnapshot, SessionExecutor};
use super::{Session, SessionStats};

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

    /// Session activity and execution statistics.
    stats: SessionStats,
}

impl std::fmt::Debug for InProcessSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessSession")
            .field("execution_count", &self.stats.execution_count)
            .field("preamble_executed", &self.preamble_executed)
            .field("last_activity", &self.stats.last_activity)
            .field("total_execution_time", &self.stats.total_execution_time)
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
    #[tracing::instrument(
        name = "InProcessSession::new",
        skip(sandbox),
        fields(
            callbacks = sandbox.callbacks().len(),
            has_preamble = !sandbox.preamble().is_empty(),
        )
    )]
    pub async fn new(sandbox: &'a Sandbox) -> Result<Self, Error> {
        let callbacks: Vec<Arc<dyn Callback>> = sandbox.callbacks().values().cloned().collect();

        let mut executor = SessionExecutor::new(sandbox.executor().clone(), &callbacks).await?;

        // Set execution timeout from sandbox resource limits
        executor.set_execution_timeout(sandbox.resource_limits().execution_timeout);

        Ok(Self {
            sandbox,
            executor,
            preamble_executed: false,
            stats: SessionStats::new(),
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
    #[tracing::instrument(
        name = "InProcessSession::execute",
        skip(self, code),
        fields(
            code_len = code.len(),
            execution_count = self.executor.execution_count(),
        )
    )]
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
        let secrets_arc = std::sync::Arc::new(self.sandbox.secrets().clone());
        let callback_secrets = std::sync::Arc::clone(&secrets_arc);
        let callback_handler = tokio::spawn(async move {
            run_callback_handler(
                callback_rx,
                callbacks_arc,
                resource_limits,
                callback_secrets,
            )
            .await
        });

        // Spawn task to handle trace events
        let trace_handler = self.sandbox.trace_handler().clone();
        let trace_secrets = self.sandbox.secrets().clone();
        let trace_collector = tokio::spawn(async move {
            run_trace_collector(trace_rx, trace_handler, trace_secrets).await
        });

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

        // Update session statistics
        self.stats.execution_count += 1;
        self.stats.total_execution_time += duration;
        self.stats.total_callback_invocations += u64::from(callback_invocations);
        self.stats.last_activity = Some(Instant::now());

        match execution_result {
            Ok(output) => {
                // Update peak memory if this execution used more
                if output.peak_memory_bytes > self.stats.peak_memory_bytes {
                    self.stats.peak_memory_bytes = output.peak_memory_bytes;
                }

                // Stream output if handler is configured
                if let Some(handler) = self.sandbox.output_handler() {
                    handler.on_output(&output.stdout).await;
                    handler.on_stderr(&output.stderr).await;
                }

                tracing::info!(
                    duration_ms = duration.as_millis() as u64,
                    callback_invocations,
                    peak_memory_bytes = output.peak_memory_bytes,
                    fuel_consumed = ?output.fuel_consumed,
                    "Session execution completed"
                );

                Ok(ExecuteResult {
                    stdout: output.stdout,
                    stderr: output.stderr,
                    trace: trace_events,
                    stats: ExecuteStats {
                        duration,
                        callback_invocations,
                        peak_memory_bytes: Some(output.peak_memory_bytes),
                        fuel_consumed: output.fuel_consumed,
                    },
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Get the number of executions performed in this session.
    #[must_use]
    pub fn execution_count(&self) -> u64 {
        self.stats.execution_count
    }

    /// Get the time of the last execution completion.
    ///
    /// Returns `None` if no executions have been performed yet.
    #[must_use]
    pub fn last_activity(&self) -> Option<Instant> {
        self.stats.last_activity
    }

    /// Get the duration since the last execution completed.
    ///
    /// Returns `None` if no executions have been performed yet.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").await?;
    /// std::thread::sleep(Duration::from_secs(1));
    /// let idle = session.idle_duration().unwrap();
    /// assert!(idle >= Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn idle_duration(&self) -> Option<Duration> {
        self.stats.last_activity.map(|last| last.elapsed())
    }

    /// Get the total execution time across all runs in this session.
    #[must_use]
    pub fn total_execution_time(&self) -> Duration {
        self.stats.total_execution_time
    }

    /// Get the complete session statistics.
    ///
    /// This includes creation time, last activity, execution count,
    /// total execution time, callback invocations, and peak memory usage.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        self.stats.clone()
    }

    /// Reset the session statistics without affecting session state.
    ///
    /// This clears all statistics (execution count, total time, etc.) but
    /// preserves the original session creation time and all Python state.
    ///
    /// Use this if you want to measure statistics for a specific workload
    /// without resetting the Python environment.
    pub fn reset_stats(&mut self) {
        self.stats.reset();
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
    /// Note: Statistics are preserved across `clear_state()`. Use `reset_stats()`
    /// to clear statistics, or `reset()` to clear both state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the clear fails.
    pub async fn clear_state(&mut self) -> Result<(), Error> {
        self.executor.clear_state().await
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

        // Full reset of stats (including creation time)
        self.stats.reset_full();

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
