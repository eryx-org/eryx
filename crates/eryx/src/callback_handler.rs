//! Shared callback and trace handling for sandbox execution.
//!
//! This module provides the callback request handler and trace event collector
//! used by both `Sandbox::execute` and `InProcessSession::execute`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;

use crate::callback::{Callback, CallbackError};
use crate::sandbox::ResourceLimits;
use crate::trace::{TraceEvent, TraceHandler};
use crate::wasm::{CallbackRequest, TraceRequest, parse_trace_event};

/// Type alias for the in-flight callback futures collection.
type InFlightCallbacks = FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>>;

/// Handle callback requests with concurrent execution.
///
/// Uses `tokio::select!` to concurrently:
/// 1. Receive new callback requests from the channel
/// 2. Poll in-flight callback futures to completion
///
/// This allows multiple callbacks to execute in parallel when Python code
/// uses `asyncio.gather()` or similar patterns.
///
/// Returns the total number of callback invocations.
pub(crate) async fn run_callback_handler(
    mut callback_rx: mpsc::Receiver<CallbackRequest>,
    callbacks_map: Arc<HashMap<String, Arc<dyn Callback>>>,
    resource_limits: ResourceLimits,
) -> u32 {
    let invocation_count = Arc::new(AtomicU32::new(0));
    let mut in_flight: InFlightCallbacks = FuturesUnordered::new();

    loop {
        tokio::select! {
            // Receive new callback requests
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

            // Poll in-flight callbacks
            Some(()) = in_flight.next(), if !in_flight.is_empty() => {
                // A callback completed, continue the loop
            }
        }
    }

    invocation_count.load(Ordering::SeqCst)
}

/// Create a future for executing a single callback.
///
/// Returns `None` if the callback limit is exceeded, the callback is not found,
/// or the arguments cannot be parsed. In these cases, an error is sent back
/// through the response channel.
fn create_callback_future(
    request: CallbackRequest,
    callbacks_map: &Arc<HashMap<String, Arc<dyn Callback>>>,
    resource_limits: &ResourceLimits,
    invocation_count: &Arc<AtomicU32>,
) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
    // Check callback limit
    let current_count = invocation_count.fetch_add(1, Ordering::SeqCst);
    if let Some(max) = resource_limits.max_callback_invocations
        && current_count >= max
    {
        let _ = request
            .response_tx
            .send(Err(format!("Callback limit exceeded ({max} invocations)")));
        return None;
    }

    // Find the callback
    let Some(callback) = callbacks_map.get(&request.name).cloned() else {
        let _ = request
            .response_tx
            .send(Err(format!("Callback '{}' not found", request.name)));
        return None;
    };

    // Parse arguments - report errors explicitly rather than silently falling back
    let args: serde_json::Value = match serde_json::from_str(&request.arguments_json) {
        Ok(v) => v,
        Err(e) => {
            let _ = request
                .response_tx
                .send(Err(format!("Invalid arguments JSON: {e}")));
            return None;
        }
    };

    // Create the future
    let timeout = resource_limits.callback_timeout;
    let fut = async move {
        let invoke_future = callback.invoke(args);

        let callback_result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, invoke_future)
                .await
                .map_or(Err(CallbackError::Timeout), |r| r)
        } else {
            invoke_future.await
        };

        let result = match callback_result {
            Ok(value) => Ok(value.to_string()),
            Err(e) => Err(e.to_string()),
        };

        // Send result back to the Python code
        let _ = request.response_tx.send(result);
    };

    Some(Box::pin(fut))
}

/// Collect trace events from the Python runtime.
///
/// Receives trace events from the channel, parses them, optionally forwards
/// to the trace handler, and collects them for the final result.
pub(crate) async fn run_trace_collector(
    mut trace_rx: mpsc::UnboundedReceiver<TraceRequest>,
    trace_handler: Option<Arc<dyn TraceHandler>>,
) -> Vec<TraceEvent> {
    let mut events = Vec::new();

    while let Some(request) = trace_rx.recv().await {
        if let Ok(event) = parse_trace_event(&request) {
            // Send to trace handler if configured
            if let Some(handler) = &trace_handler {
                handler.on_trace(event.clone()).await;
            }
            events.push(event);
        }
    }

    events
}
