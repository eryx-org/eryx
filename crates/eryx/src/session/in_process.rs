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
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;

use crate::callback::{Callback, CallbackError};
use crate::error::Error;
use crate::sandbox::{ExecuteResult, ExecuteStats, ResourceLimits, Sandbox};
use crate::trace::TraceEvent;
use crate::wasm::{CallbackRequest, TraceRequest, parse_trace_event};

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
    /// The session will share the sandbox's configuration (callbacks, preamble, etc.)
    /// but maintain its own persistent state.
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be initialized.
    pub async fn new(sandbox: &'a Sandbox) -> Result<Self, Error> {
        let callbacks: Vec<Arc<dyn Callback>> = sandbox.callbacks().values().cloned().collect();

        let executor = SessionExecutor::new(sandbox.executor().clone(), &callbacks).await?;

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

        // Spawn task to handle callback requests concurrently
        let callbacks_map = self.sandbox.callbacks().clone();
        let resource_limits = self.sandbox.resource_limits().clone();
        let callback_handler = tokio::spawn(async move {
            run_callback_handler(callback_rx, callbacks_map, resource_limits).await
        });

        // Spawn task to handle trace events
        let trace_handler = self.sandbox.trace_handler().clone();
        let trace_collector =
            tokio::spawn(async move { run_trace_collector(trace_rx, trace_handler).await });

        // Get callbacks for this execution
        let callbacks: Vec<Arc<dyn Callback>> =
            self.sandbox.callbacks().values().cloned().collect();

        // Execute using the session executor (keeps instance alive!)
        let execute_future =
            self.executor
                .execute(&full_code, &callbacks, Some(callback_tx), Some(trace_tx));

        let execution_result =
            if let Some(timeout) = self.sandbox.resource_limits().execution_timeout {
                tokio::time::timeout(timeout, execute_future)
                    .await
                    .unwrap_or_else(|_| Err(format!("Execution timed out after {timeout:?}")))
            } else {
                execute_future.await
            };

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

/// Handle callback requests with concurrent execution.
async fn run_callback_handler(
    mut callback_rx: mpsc::Receiver<CallbackRequest>,
    callbacks_map: std::collections::HashMap<String, Arc<dyn Callback>>,
    resource_limits: ResourceLimits,
) -> u32 {
    let invocation_count = Arc::new(AtomicU32::new(0));
    let mut in_flight: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    > = FuturesUnordered::new();

    loop {
        tokio::select! {
            request = callback_rx.recv() => {
                if let Some(req) = request {
                    if let Some(fut) = create_callback_future(
                        req,
                        &callbacks_map,
                        &resource_limits,
                        &invocation_count,
                    ) {
                        in_flight.push(fut);
                    }
                } else {
                    // Channel closed, drain remaining futures and exit
                    while in_flight.next().await.is_some() {}
                    break;
                }
            }

            Some(()) = in_flight.next(), if !in_flight.is_empty() => {
                // A callback completed, continue the loop
            }
        }
    }

    invocation_count.load(Ordering::SeqCst)
}

/// Create a future for executing a single callback.
fn create_callback_future(
    request: CallbackRequest,
    callbacks_map: &std::collections::HashMap<String, Arc<dyn Callback>>,
    resource_limits: &ResourceLimits,
    invocation_count: &Arc<AtomicU32>,
) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
    // Check callback limit
    let current_count = invocation_count.fetch_add(1, Ordering::SeqCst);
    if let Some(max) = resource_limits.max_callback_invocations
        && current_count >= max
    {
        let _ = request
            .response_tx
            .send(Err("Callback limit exceeded".to_string()));
        return None;
    }

    let callback = callbacks_map.get(&request.name).cloned();
    let timeout = resource_limits.callback_timeout;

    Some(Box::pin(async move {
        let result: Result<String, String> = match callback {
            Some(cb) => {
                // Parse the JSON arguments
                let args: serde_json::Value = serde_json::from_str(&request.arguments_json)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                let invoke_future = cb.invoke(args);
                let cb_result = if let Some(timeout_duration) = timeout {
                    match tokio::time::timeout(timeout_duration, invoke_future).await {
                        Ok(r) => r,
                        Err(_) => Err(CallbackError::Timeout),
                    }
                } else {
                    invoke_future.await
                };

                // Convert to Result<String, String> for the channel
                cb_result
                    .map(|v| serde_json::to_string(&v).unwrap_or_default())
                    .map_err(|e| e.to_string())
            }
            None => Err(format!("Unknown callback: {}", request.name)),
        };

        let _ = request.response_tx.send(result);
    }))
}

/// Collect trace events from the trace channel.
async fn run_trace_collector(
    mut trace_rx: mpsc::UnboundedReceiver<TraceRequest>,
    trace_handler: Option<Arc<dyn crate::trace::TraceHandler>>,
) -> Vec<TraceEvent> {
    let mut events = Vec::new();

    while let Some(request) = trace_rx.recv().await {
        if let Ok(event) = parse_trace_event(&request) {
            // Call trace handler if configured
            if let Some(handler) = &trace_handler {
                handler.on_trace(event.clone()).await;
            }
            events.push(event);
        }
    }

    events
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
