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
//! assert_eq!(result.stdout, b"3\n");
//!
//! // Reset clears all state
//! session.reset().await?;
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::callback::Callback;
use crate::callback_handler::{
    run_callback_handler, run_net_handler, run_output_collector, run_trace_collector,
};
use crate::error::Error;
use crate::net::ConnectionManager;
use crate::replay::{CallbackJournal, ReplayState, wrap_callbacks};
use crate::sandbox::{ExecuteResult, ExecuteStats, ReplayOutcome, Sandbox, handler_task_failed};
use crate::wasm::{CallbackRequest, NetRequest, OutputRequest, TraceRequest};

use super::Session;
use super::executor::{PythonStateSnapshot, SessionExecutor};

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
    /// The session will share the sandbox's configuration (callbacks, preamble,
    /// and resource limits) but maintain its own persistent state. Its memory
    /// limit is applied while the WASM instance is created, while timeout and
    /// fuel limits apply to each execution and remain in force after [`Self::reset`]. Callback
    /// timeout and invocation-count limits continue to be enforced by the
    /// callback handler.
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

        // Construct the store with the sandbox's execution, memory, and VFS
        // limits so memory growth is constrained during instantiation, not
        // only execution. Callback limits remain handler-owned below.
        let executor = SessionExecutor::new_with_limits(
            sandbox.executor().clone(),
            &callbacks,
            sandbox.resource_limits(),
        )
        .await?;

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
        self.recover_if_cancelled().await?;
        let preamble = self.take_preamble();
        run_code(self.sandbox, &mut self.executor, code, preamble, None).await
    }

    /// Start over if a cancelled call took the instance down with it.
    ///
    /// A call whose future is dropped mid-run (e.g. by `tokio::select!` or a
    /// timeout) loses the instance it was running on. Reset so the session
    /// stays usable, re-running the preamble on the next call.
    async fn recover_if_cancelled(&mut self) -> Result<(), Error> {
        let callbacks: Vec<Arc<dyn Callback>> =
            self.sandbox.callbacks().values().cloned().collect();
        if self.executor.recover_lost_store(&callbacks).await? {
            self.preamble_executed = false;
        }
        Ok(())
    }

    /// The sandbox preamble on the session's first call, else `""`.
    fn take_preamble(&mut self) -> &'a str {
        if self.preamble_executed {
            return "";
        }
        self.preamble_executed = true;
        self.sandbox.preamble()
    }

    /// Execute Python code in the session with callback-result replay and
    /// journaling, keeping session state between calls.
    ///
    /// The session counterpart of [`Sandbox::execute_with_journal`]: callbacks
    /// matching `journal` replay their recorded results, and the returned
    /// [`ReplayOutcome`] carries the journal recorded during this call. Each
    /// call uses fresh replay state, so different calls can pass different
    /// journals.
    ///
    /// # Suspension and rollback
    ///
    /// A suspension (or a timeout, fuel exhaustion or trap) halts the guest,
    /// which would leave the session unusable. The call therefore runs under
    /// [`SessionExecutor::run_with_rollback`]: the session state is snapshotted
    /// first (one [`snapshot_state`](Self::snapshot_state) per call, skipped
    /// when the sandbox has no callbacks), and if the run halts, the instance is
    /// reset and the snapshot restored. The session is then back where it was
    /// before the call, so resuming with the returned journal in the same
    /// session does not apply the replayed prefix's effects on Python globals
    /// twice. VFS/volume writes and network calls are not rolled back, and only
    /// serializable globals survive (as with `snapshot_state`).
    ///
    /// If the snapshot fails (e.g. it exceeds the size limit), a warning is
    /// logged and the call runs unprotected: a halt still resets the session so
    /// it stays usable, but its state is lost. If the rollback itself fails, the
    /// session is unusable: the error is returned in [`ReplayOutcome::result`]
    /// and [`ReplayOutcome::suspended`] is cleared so the call doesn't look
    /// resumable here. The journal is still valid for resuming in a fresh
    /// session.
    ///
    /// # Preamble
    ///
    /// The sandbox preamble runs as part of the session's first call. If that
    /// call is journaled, callbacks the preamble invokes are journaled too. A
    /// rollback of that first call re-runs the preamble on the next call, but a
    /// rollback of a later call does not, so any preamble setup that cannot be
    /// serialized is lost.
    ///
    /// # Security
    ///
    /// Replayed journal entries are returned to Python verbatim. Only replay
    /// journals from a trusted source; see the [`replay`](crate::replay) module.
    pub async fn execute_with_journal(
        &mut self,
        code: &str,
        journal: Option<CallbackJournal>,
    ) -> ReplayOutcome {
        let state = ReplayState::shared(code, journal);
        if let Err(e) = self.recover_if_cancelled().await {
            return ReplayOutcome::from_state(&state, code, Err(e));
        }
        let preamble_executed = self.preamble_executed;
        let preamble = self.take_preamble();
        let sandbox = self.sandbox;
        let callbacks: Vec<Arc<dyn Callback>> = sandbox.callbacks().values().cloned().collect();

        let run = self
            .executor
            .run_with_rollback(
                &callbacks,
                || lock_suspended(&state),
                async |executor| {
                    run_code(sandbox, executor, code, preamble, Some(Arc::clone(&state))).await
                },
            )
            .await;

        match run {
            Ok((result, rolled_back)) => {
                if rolled_back {
                    self.preamble_executed = preamble_executed;
                }
                ReplayOutcome::from_state(&state, code, result)
            }
            Err(rollback_error) => {
                let mut outcome = ReplayOutcome::from_state(&state, code, Err(rollback_error));
                outcome.suspended = None;
                outcome
            }
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
        self.recover_if_cancelled().await?;
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
        self.recover_if_cancelled().await?;
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
        self.recover_if_cancelled().await?;
        self.executor.clear_state().await
    }
}

/// Aborts its task when dropped, so a cancelled [`run_code`] doesn't leave its
/// handler tasks (and any callbacks they are running) behind.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether a callback suspended the run that used `state`.
fn lock_suspended(state: &Mutex<ReplayState>) -> bool {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .suspended()
        .is_some()
}

/// Run `code` (after `preamble`) on `executor` with `sandbox`'s callbacks,
/// handlers and limits, wrapping callbacks for replay when `replay_state` is set.
#[tracing::instrument(
    name = "InProcessSession::execute",
    skip(sandbox, executor, code, preamble, replay_state),
    fields(
        code_len = code.len(),
        execution_count = executor.execution_count(),
    )
)]
async fn run_code(
    sandbox: &Sandbox,
    executor: &mut SessionExecutor,
    code: &str,
    preamble: &str,
    replay_state: Option<Arc<Mutex<ReplayState>>>,
) -> Result<ExecuteResult, Error> {
    let start = Instant::now();

    // Create channel for callback requests
    let (callback_tx, callback_rx) = mpsc::channel::<CallbackRequest>(32);

    // Wrap each callback with a replay wrapper when journaling/replay is
    // enabled, otherwise use the registered callbacks directly.
    let callbacks_arc = match &replay_state {
        Some(state) => Arc::new(wrap_callbacks(sandbox.callbacks(), state)),
        None => sandbox.callbacks_arc(),
    };
    let callbacks: Vec<Arc<dyn Callback>> = callbacks_arc.values().cloned().collect();

    // Spawn task to handle callback requests concurrently
    let resource_limits = sandbox.resource_limits().clone();
    let secrets_arc = std::sync::Arc::new(sandbox.secrets().clone());
    let callback_secrets = std::sync::Arc::clone(&secrets_arc);
    let mut callback_handler = AbortOnDrop(tokio::spawn(async move {
        run_callback_handler(
            callback_rx,
            callbacks_arc,
            resource_limits,
            callback_secrets,
        )
        .await
    }));

    // Create the trace channel and collector only when tracing is enabled.
    let tracing_enabled = sandbox.tracing_enabled();
    let (trace_tx, trace_collector) = if tracing_enabled {
        let (trace_tx, trace_rx) = mpsc::unbounded_channel::<TraceRequest>();
        let trace_handler = sandbox.trace_handler().clone();
        let collect_trace = sandbox.trace_collection_enabled();
        let trace_secrets = sandbox.secrets().clone();
        let trace_collector = AbortOnDrop(tokio::spawn(async move {
            run_trace_collector(trace_rx, trace_handler, collect_trace, trace_secrets).await
        }));
        (Some(trace_tx), Some(trace_collector))
    } else {
        (None, None)
    };

    // Spawn task to handle streaming output
    let (output_tx, output_rx) = mpsc::unbounded_channel::<OutputRequest>();
    let output_handler_ref = sandbox.output_handler().clone();
    let output_secrets = sandbox.secrets().clone();
    let scrub_stdout = sandbox.scrub_stdout();
    let scrub_stderr = sandbox.scrub_stderr();
    let mut output_collector = AbortOnDrop(tokio::spawn(async move {
        run_output_collector(
            output_rx,
            output_handler_ref,
            output_secrets,
            scrub_stdout,
            scrub_stderr,
        )
        .await
    }));

    // Spawn the network handler if networking is enabled
    let (net_tx, net_handler) = match sandbox.net_config() {
        Some(config) => {
            let (tx, rx) = mpsc::channel::<NetRequest>(32);
            let manager = ConnectionManager::new(config.clone(), sandbox.secrets().clone());
            let handler = AbortOnDrop(tokio::spawn(run_net_handler(rx, manager)));
            (Some(tx), Some(handler))
        }
        None => (None, None),
    };

    // Execute using the session executor (keeps instance alive!)
    // Timeout is handled via epoch-based interruption inside the executor
    let mut execute_builder = executor
        .execute(code)
        .with_preamble(preamble)
        .with_callbacks(&callbacks, callback_tx)
        .with_output_streaming(output_tx);
    if let Some(trace_tx) = trace_tx {
        execute_builder = execute_builder.with_tracing(trace_tx);
    }
    if let Some(net_tx) = net_tx {
        execute_builder = execute_builder.with_network(net_tx);
    }
    let execution_result = execute_builder.run().await;

    // Wait for the handler tasks to complete
    let callback_invocations = (&mut callback_handler.0).await;
    let trace_events = match trace_collector {
        Some(mut trace_collector) => (&mut trace_collector.0).await,
        None => Ok(Vec::new()),
    };
    let output_result = (&mut output_collector.0).await;
    // Connections don't outlive the call, as with `Sandbox::execute`.
    let net_result = match net_handler {
        Some(mut net_handler) => (&mut net_handler.0).await,
        None => Ok(()),
    };

    // A handler task only fails by panicking (e.g. in a user callback or
    // handler), which loses callback results, trace events or output.
    let callback_invocations =
        callback_invocations.map_err(|e| handler_task_failed("callback handler", &e))?;
    let trace_events = trace_events.map_err(|e| handler_task_failed("trace collector", &e))?;
    net_result.map_err(|e| handler_task_failed("network handler", &e))?;
    output_result.map_err(|e| handler_task_failed("output handler", &e))?;

    let duration = start.elapsed();

    match execution_result {
        Ok(output) => {
            tracing::info!(
                duration_ms = duration.as_millis() as u64,
                callback_invocations,
                peak_memory_bytes = output.peak_memory_bytes,
                fuel_consumed = ?output.fuel_consumed,
                "Session execution completed"
            );

            let output = sandbox.scrub_output(output);

            Ok(ExecuteResult {
                stdout: output.stdout,
                stderr: output.stderr,
                trace: trace_events,
                result: output.result,
                result_error: output.result_error,
                state_was_reset: output.state_was_reset,
                stream_truncated: output.stream_truncated,
                stats: ExecuteStats {
                    duration,
                    callback_invocations,
                    peak_memory_bytes: Some(output.peak_memory_bytes),
                    fuel_consumed: output.fuel_consumed,
                },
            })
        }
        Err(error) => Err(sandbox.scrub_error(error)),
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
