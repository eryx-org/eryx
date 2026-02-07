//! Shared callback, network, and trace handling for sandbox execution.
//!
//! This module provides the callback request handler, network request handler,
//! and trace event collector used by both `Sandbox::execute` and
//! `InProcessSession::execute`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;

use crate::callback::{Callback, CallbackError};
use crate::net::ConnectionManager;
use crate::sandbox::ResourceLimits;
use crate::secrets::SecretConfig;
use crate::trace::{OutputHandler, TraceEvent, TraceEventKind, TraceHandler};
use crate::wasm::{CallbackRequest, NetRequest, OutputRequest, TraceRequest, parse_trace_event};

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
#[tracing::instrument(
    skip(callback_rx, callbacks_map, resource_limits, secrets),
    fields(
        available_callbacks = callbacks_map.len(),
        max_invocations = ?resource_limits.max_callback_invocations,
    )
)]
pub async fn run_callback_handler(
    mut callback_rx: mpsc::Receiver<CallbackRequest>,
    callbacks_map: Arc<HashMap<String, Arc<dyn Callback>>>,
    resource_limits: ResourceLimits,
    secrets: Arc<HashMap<String, SecretConfig>>,
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
                        &secrets,
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
    secrets: &Arc<HashMap<String, SecretConfig>>,
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
    let secrets = Arc::clone(secrets);
    let fut = async move {
        let invoke_future = callback.invoke(args);

        let callback_result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, invoke_future)
                .await
                .map_or(Err(CallbackError::Timeout), |r| r)
        } else {
            invoke_future.await
        };

        // Scrub secret placeholders from callback results
        let result = match callback_result {
            Ok(value) => Ok(crate::secrets::scrub_placeholders(
                &value.to_string(),
                &secrets,
            )),
            Err(e) => Err(crate::secrets::scrub_placeholders(&e.to_string(), &secrets)),
        };

        // Send result back to the Python code
        let _ = request.response_tx.send(result);
    };

    Some(Box::pin(fut))
}

/// Scrub secret placeholders from a trace event.
fn scrub_trace_event(event: &mut TraceEvent, secrets: &HashMap<String, SecretConfig>) {
    if secrets.is_empty() {
        return;
    }

    // Scrub the event kind
    match &mut event.event {
        TraceEventKind::Exception { message } => {
            *message = crate::secrets::scrub_placeholders(message, secrets);
        }
        TraceEventKind::Call { function } | TraceEventKind::Return { function } => {
            *function = crate::secrets::scrub_placeholders(function, secrets);
        }
        TraceEventKind::CallbackStart { name } | TraceEventKind::CallbackEnd { name, .. } => {
            *name = crate::secrets::scrub_placeholders(name, secrets);
        }
        TraceEventKind::Line => {}
    }

    // Scrub context if present
    if let Some(ctx) = &event.context {
        let ctx_str = ctx.to_string();
        let scrubbed = crate::secrets::scrub_placeholders(&ctx_str, secrets);
        if scrubbed != ctx_str {
            // Re-parse the scrubbed JSON; fall back to string value on parse failure
            let scrubbed_value =
                serde_json::from_str(&scrubbed).unwrap_or(serde_json::Value::String(scrubbed));
            event.context = Some(scrubbed_value);
        }
    }
}

/// Collect trace events from the Python runtime.
///
/// Receives trace events from the channel, parses them, optionally forwards
/// to the trace handler, and collects them for the final result.
///
/// Secret placeholders are scrubbed from events before storing/forwarding.
#[tracing::instrument(
    skip(trace_rx, trace_handler, secrets),
    fields(has_handler = trace_handler.is_some())
)]
pub(crate) async fn run_trace_collector(
    mut trace_rx: mpsc::UnboundedReceiver<TraceRequest>,
    trace_handler: Option<Arc<dyn TraceHandler>>,
    secrets: HashMap<String, SecretConfig>,
) -> Vec<TraceEvent> {
    let mut events = Vec::new();

    while let Some(request) = trace_rx.recv().await {
        if let Ok(mut event) = parse_trace_event(&request) {
            // Scrub secret placeholders from the event
            scrub_trace_event(&mut event, &secrets);

            // Send to trace handler if configured
            if let Some(handler) = &trace_handler {
                handler.on_trace(event.clone()).await;
            }
            events.push(event);
        }
    }

    events
}

/// Collect and dispatch streaming output (stdout/stderr) from the Python runtime.
///
/// Receives output chunks via the channel and dispatches them to the
/// `OutputHandler` in real-time. Optionally scrubs secret placeholders.
#[tracing::instrument(
    skip(output_rx, output_handler, secrets),
    fields(has_handler = output_handler.is_some())
)]
pub(crate) async fn run_output_collector(
    mut output_rx: mpsc::UnboundedReceiver<OutputRequest>,
    output_handler: Option<Arc<dyn OutputHandler>>,
    secrets: HashMap<String, SecretConfig>,
    scrub_stdout: bool,
    scrub_stderr: bool,
) {
    while let Some(request) = output_rx.recv().await {
        if let Some(handler) = &output_handler {
            let should_scrub = match request.stream {
                0 => scrub_stdout,
                1 => scrub_stderr,
                _ => false,
            };

            let data = if should_scrub && !secrets.is_empty() {
                crate::secrets::scrub_placeholders(&request.data, &secrets)
            } else {
                request.data
            };

            match request.stream {
                0 => handler.on_output(&data).await,
                1 => handler.on_stderr(&data).await,
                _ => {}
            }
        }
    }
}

/// Handle network requests (TCP and TLS) from Python code.
///
/// Owns a [`ConnectionManager`] and processes TCP/TLS requests received through
/// the channel. This allows async network operations to work with wasmtime's
/// synchronous accessor pattern.
#[tracing::instrument(skip(net_rx, manager))]
pub(crate) async fn run_net_handler(
    mut net_rx: mpsc::Receiver<NetRequest>,
    mut manager: ConnectionManager,
) {
    while let Some(request) = net_rx.recv().await {
        match request {
            // TCP operations
            NetRequest::TcpConnect {
                host,
                port,
                response_tx,
            } => {
                let result = manager.tcp_connect(&host, port).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TcpRead {
                handle,
                len,
                response_tx,
            } => {
                let result = manager.tcp_read(handle, len).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TcpWrite {
                handle,
                data,
                response_tx,
            } => {
                let result = manager.tcp_write(handle, &data).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TcpClose { handle } => {
                manager.tcp_close(handle);
            }

            // TLS operations
            NetRequest::TlsUpgrade {
                tcp_handle,
                hostname,
                response_tx,
            } => {
                let result = manager.tls_upgrade(tcp_handle, &hostname).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TlsRead {
                handle,
                len,
                response_tx,
            } => {
                let result = manager.tls_read(handle, len).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TlsWrite {
                handle,
                data,
                response_tx,
            } => {
                let result = manager.tls_write(handle, &data).await;
                let _ = response_tx.send(result);
            }
            NetRequest::TlsClose { handle } => {
                manager.tls_close(handle);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_secrets() -> HashMap<String, SecretConfig> {
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            SecretConfig {
                real_value: "real-secret".to_string(),
                placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
                allowed_hosts: vec![],
            },
        );
        secrets
    }

    #[test]
    fn test_trace_event_exception_scrubbed() {
        let secrets = test_secrets();
        let mut event = TraceEvent {
            lineno: 1,
            event: TraceEventKind::Exception {
                message: "Error: ERYX_SECRET_PLACEHOLDER_abc123 is invalid".to_string(),
            },
            context: None,
        };

        scrub_trace_event(&mut event, &secrets);

        match &event.event {
            TraceEventKind::Exception { message } => {
                assert_eq!(message, "Error: [REDACTED] is invalid");
                assert!(!message.contains("ERYX_SECRET_PLACEHOLDER"));
            }
            _ => panic!("Expected Exception event"),
        }
    }

    #[test]
    fn test_trace_event_context_scrubbed() {
        let secrets = test_secrets();
        let mut event = TraceEvent {
            lineno: 1,
            event: TraceEventKind::Line,
            context: Some(json!({
                "key": "ERYX_SECRET_PLACEHOLDER_abc123",
                "other": "safe"
            })),
        };

        scrub_trace_event(&mut event, &secrets);

        let ctx = event.context.unwrap();
        let ctx_str = ctx.to_string();
        assert!(!ctx_str.contains("ERYX_SECRET_PLACEHOLDER"));
        assert!(ctx_str.contains("[REDACTED]"));
        assert!(ctx_str.contains("safe"));
    }

    #[test]
    fn test_trace_event_call_function_scrubbed() {
        let secrets = test_secrets();
        let mut event = TraceEvent {
            lineno: 1,
            event: TraceEventKind::Call {
                function: "fn_ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
            },
            context: None,
        };

        scrub_trace_event(&mut event, &secrets);

        match &event.event {
            TraceEventKind::Call { function } => {
                assert_eq!(function, "fn_[REDACTED]");
            }
            _ => panic!("Expected Call event"),
        }
    }

    #[test]
    fn test_trace_event_callback_name_scrubbed() {
        let secrets = test_secrets();
        let mut event = TraceEvent {
            lineno: 1,
            event: TraceEventKind::CallbackStart {
                name: "cb_ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
            },
            context: None,
        };

        scrub_trace_event(&mut event, &secrets);

        match &event.event {
            TraceEventKind::CallbackStart { name } => {
                assert_eq!(name, "cb_[REDACTED]");
            }
            _ => panic!("Expected CallbackStart event"),
        }
    }

    #[test]
    fn test_trace_event_no_secrets_passthrough() {
        let secrets = HashMap::new();
        let mut event = TraceEvent {
            lineno: 1,
            event: TraceEventKind::Exception {
                message: "normal error".to_string(),
            },
            context: None,
        };

        scrub_trace_event(&mut event, &secrets);

        match &event.event {
            TraceEventKind::Exception { message } => {
                assert_eq!(message, "normal error");
            }
            _ => panic!("Expected Exception event"),
        }
    }
}
