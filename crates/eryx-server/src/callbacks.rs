//! Callback bridge between gRPC and the eryx sandbox.
//!
//! Converts protobuf [`CallbackDeclaration`]s into eryx [`DynamicCallback`]s
//! that send callback requests over the gRPC response stream and await
//! responses from the client via per-request oneshot channels.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use eryx::{CallbackError, DynamicCallback};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::proto::eryx::v1::{CallbackDeclaration, CallbackRequest, ServerMessage, server_message};

/// Result of a callback invocation returned by the Go client.
#[derive(Debug)]
pub enum CallbackResult {
    /// Successful JSON result.
    Ok(String),
    /// Error message.
    Err(String),
    /// The client asked to suspend execution ("retry later"). The string is the
    /// opaque reason. Surfaced as [`CallbackError::Suspend`] so replay-aware
    /// execution halts the guest and reports the suspension to the caller.
    Suspend(String),
}

/// Error returned by callbacks once the client can no longer respond.
const CLIENT_CLOSED: &str =
    "client closed the request stream; callback responses can no longer arrive";

/// Map of pending callback requests, keyed by request ID.
pub type PendingCallbacks = Arc<DashMap<String, oneshot::Sender<CallbackResult>>>;

/// Build eryx [`DynamicCallback`]s from protobuf declarations.
///
/// Each callback, when invoked by Python:
/// 1. Generates a UUID `request_id`
/// 2. Sends a [`CallbackRequest`] over the gRPC response channel
/// 3. Inserts a oneshot sender into the pending map
/// 4. Awaits the oneshot receiver for the Go client's response
/// 5. Returns the parsed JSON result or a [`CallbackError`]
///
/// `inbound_closed` is cancelled once the client stops sending (half-close or
/// disconnect). No response can arrive after that, so a callback invoked then,
/// or still waiting, fails immediately instead of waiting for its timeout.
pub fn build_callbacks(
    declarations: &[CallbackDeclaration],
    grpc_tx: mpsc::Sender<ServerMessage>,
    pending: PendingCallbacks,
    inbound_closed: CancellationToken,
) -> Vec<Box<dyn eryx::Callback>> {
    tracing::debug!(callback_count = declarations.len(), "building callbacks");
    declarations
        .iter()
        .map(|decl| {
            let tx = grpc_tx.clone();
            let pending = Arc::clone(&pending);
            let inbound_closed = inbound_closed.clone();
            let name = decl.name.clone();

            let mut builder = DynamicCallback::builder(
                decl.name.clone(),
                decl.description.clone(),
                move |args| {
                    let tx = tx.clone();
                    let pending = Arc::clone(&pending);
                    let inbound_closed = inbound_closed.clone();
                    let name = name.clone();

                    Box::pin(async move {
                        let started = Instant::now();
                        let request_id = Uuid::new_v4().to_string();
                        let arguments_json = serde_json::to_string(&args)
                            .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?;

                        if inbound_closed.is_cancelled() {
                            tracing::warn!(
                                callback_name = %name,
                                "callback invoked after the client stopped sending"
                            );
                            return Err(CallbackError::ExecutionFailed(CLIENT_CLOSED.to_string()));
                        }

                        tracing::info!(
                            callback_name = %name,
                            %request_id,
                            args_len = arguments_json.len(),
                            "callback invoked"
                        );

                        // Create oneshot channel for the response.
                        let (resp_tx, resp_rx) = oneshot::channel();
                        pending.insert(request_id.clone(), resp_tx);

                        // Capture the current span's trace context so the Go
                        // client can create child spans for callback handling.
                        let trace_context = {
                            let cx = tracing::Span::current().context();
                            let mut fields = HashMap::new();
                            opentelemetry::global::get_text_map_propagator(|propagator| {
                                propagator.inject_context(&cx, &mut fields);
                            });
                            fields.remove("traceparent").unwrap_or_default()
                        };

                        // Send the callback request to the Go client.
                        let msg = ServerMessage {
                            message: Some(server_message::Message::CallbackRequest(
                                CallbackRequest {
                                    request_id: request_id.clone(),
                                    name: name.clone(),
                                    arguments_json,
                                    trace_context,
                                },
                            )),
                        };

                        tx.send(msg).await.map_err(|e| {
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            tracing::warn!(
                                callback_name = %name,
                                %request_id,
                                error = %e,
                                waiting_ms = elapsed_ms,
                                "gRPC response channel closed"
                            );
                            CallbackError::ExecutionFailed(
                                "gRPC response channel closed".to_string(),
                            )
                        })?;

                        // Wait for the Go client to respond. `biased` so a
                        // response delivered just before the client stopped
                        // sending still wins.
                        let result = tokio::select! {
                            biased;
                            result = resp_rx => result,
                            () = inbound_closed.cancelled() => {
                                pending.remove(&request_id);
                                tracing::warn!(
                                    callback_name = %name,
                                    %request_id,
                                    "client stopped sending before responding to callback"
                                );
                                return Err(CallbackError::ExecutionFailed(
                                    CLIENT_CLOSED.to_string(),
                                ));
                            }
                        };
                        let result = result.map_err(|e| {
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            tracing::warn!(
                                callback_name = %name,
                                %request_id,
                                error = %e,
                                waiting_ms = elapsed_ms,
                                "callback response channel dropped"
                            );
                            CallbackError::ExecutionFailed(
                                "callback response channel dropped".to_string(),
                            )
                        })?;

                        let duration_ms = started.elapsed().as_millis() as u64;

                        if duration_ms > 5000 {
                            tracing::warn!(
                                callback_name = %name,
                                %request_id,
                                duration_ms,
                                "slow callback: round-trip exceeded 5s"
                            );
                        }

                        // Clean up the pending entry (already removed by dispatch).
                        match result {
                            CallbackResult::Ok(ref json_str) => {
                                tracing::debug!(
                                    callback_name = %name,
                                    %request_id,
                                    duration_ms,
                                    success = true,
                                    "callback response received"
                                );
                                serde_json::from_str(json_str).map_err(|e| {
                                    CallbackError::ExecutionFailed(format!(
                                        "invalid callback result JSON: {e}"
                                    ))
                                })
                            }
                            CallbackResult::Err(err) => {
                                tracing::debug!(
                                    callback_name = %name,
                                    %request_id,
                                    duration_ms,
                                    success = false,
                                    error = %err.chars().take(200).collect::<String>(),
                                    "callback response received"
                                );
                                Err(CallbackError::ExecutionFailed(err))
                            }
                            CallbackResult::Suspend(reason) => {
                                tracing::info!(
                                    callback_name = %name,
                                    %request_id,
                                    duration_ms,
                                    "callback requested suspension"
                                );
                                Err(CallbackError::Suspend(reason))
                            }
                        }
                    })
                },
            );

            // Add parameter definitions to the schema.
            for param in &decl.parameters {
                builder = builder.param(
                    param.name.clone(),
                    param.json_type.clone(),
                    param.description.clone(),
                    param.required,
                );
            }

            Box::new(builder.build()) as Box<dyn eryx::Callback>
        })
        .collect()
}

/// Validate callback declarations before they are registered.
///
/// Each name must be an identifier or a dot-separated path of identifiers
/// (`http.get` is exposed as a namespace), and names must be unique.
///
/// # Errors
///
/// Returns a message describing the first invalid declaration.
pub fn validate_declarations(declarations: &[CallbackDeclaration]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for decl in declarations {
        if !decl.name.split('.').all(crate::service::is_identifier) {
            return Err(format!(
                "invalid callback name {:?}: must be an identifier or dot-separated \
                 identifiers ([A-Za-z_][A-Za-z0-9_]*)",
                decl.name
            ));
        }
        if !names.insert(decl.name.as_str()) {
            return Err(format!("duplicate callback name {:?}", decl.name));
        }
    }
    Ok(())
}

/// Dispatch a `CallbackResponse` to its pending oneshot sender.
///
/// Returns `true` if the response was delivered, `false` if no pending
/// request was found for the given `request_id`.
pub fn dispatch_callback_response(
    pending: &PendingCallbacks,
    request_id: &str,
    result: CallbackResult,
) -> bool {
    if let Some((_, sender)) = pending.remove(request_id) {
        tracing::debug!(request_id, "dispatching callback response");
        // Ignoring send error: the callback handler may have timed out.
        let _ = sender.send(result);
        true
    } else {
        tracing::warn!(request_id, "no pending callback found for response");
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::proto::eryx::v1::{CallbackDeclaration, ParameterDeclaration};
    use serde_json::json;

    fn make_echo_declaration() -> CallbackDeclaration {
        CallbackDeclaration {
            name: "echo".to_string(),
            description: "Echoes the input".to_string(),
            parameters: vec![ParameterDeclaration {
                name: "message".to_string(),
                json_type: "string".to_string(),
                description: "The message to echo".to_string(),
                required: true,
            }],
        }
    }

    #[tokio::test]
    async fn build_callbacks_creates_correct_count() {
        let (tx, _rx) = mpsc::channel(16);
        let pending: PendingCallbacks = Arc::new(DashMap::new());

        let decls = vec![
            make_echo_declaration(),
            CallbackDeclaration {
                name: "noop".to_string(),
                description: "Does nothing".to_string(),
                parameters: vec![],
            },
        ];

        let callbacks = build_callbacks(&decls, tx, pending, CancellationToken::new());
        assert_eq!(callbacks.len(), 2);
        assert_eq!(callbacks[0].name(), "echo");
        assert_eq!(callbacks[1].name(), "noop");
    }

    #[tokio::test]
    async fn callback_sends_request_and_receives_response() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending: PendingCallbacks = Arc::new(DashMap::new());

        let callbacks = build_callbacks(
            &[make_echo_declaration()],
            tx,
            Arc::clone(&pending),
            CancellationToken::new(),
        );
        let echo = &callbacks[0];

        // Spawn a task that responds to the callback request.
        let pending_clone = Arc::clone(&pending);
        let responder = tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            if let Some(server_message::Message::CallbackRequest(req)) = msg.message {
                assert_eq!(req.name, "echo");
                dispatch_callback_response(
                    &pending_clone,
                    &req.request_id,
                    CallbackResult::Ok(r#"{"echoed":"hello"}"#.to_string()),
                );
            } else {
                panic!("expected CallbackRequest");
            }
        });

        let result = echo.invoke(json!({"message": "hello"})).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({"echoed": "hello"}));

        responder.await.unwrap();
    }

    #[tokio::test]
    async fn callback_returns_error_from_client() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending: PendingCallbacks = Arc::new(DashMap::new());

        let callbacks = build_callbacks(
            &[make_echo_declaration()],
            tx,
            Arc::clone(&pending),
            CancellationToken::new(),
        );
        let echo = &callbacks[0];

        let pending_clone = Arc::clone(&pending);
        let responder = tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            if let Some(server_message::Message::CallbackRequest(req)) = msg.message {
                dispatch_callback_response(
                    &pending_clone,
                    &req.request_id,
                    CallbackResult::Err("datasource not found".to_string()),
                );
            }
        });

        let result = echo.invoke(json!({"message": "hello"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CallbackError::ExecutionFailed(ref msg) if msg.contains("datasource not found"))
        );

        responder.await.unwrap();
    }

    #[tokio::test]
    async fn callback_fails_fast_once_client_stopped_sending() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending: PendingCallbacks = Arc::new(DashMap::new());
        let closed = CancellationToken::new();
        let callbacks = build_callbacks(
            &[make_echo_declaration()],
            tx,
            Arc::clone(&pending),
            closed.clone(),
        );

        // Already waiting when the client stops sending.
        let waiting = callbacks[0].invoke(json!({"message": "a"}));
        let cancel = async {
            assert!(rx.recv().await.is_some());
            closed.cancel();
        };
        let (result, ()) = tokio::join!(waiting, cancel);
        assert!(matches!(result, Err(CallbackError::ExecutionFailed(_))));

        // Invoked afterwards: fails without sending a request.
        let result = callbacks[0].invoke(json!({"message": "b"})).await;
        assert!(matches!(result, Err(CallbackError::ExecutionFailed(_))));
        assert!(rx.try_recv().is_err());
        assert!(pending.is_empty());
    }

    #[test]
    fn validate_declarations_rejects_bad_and_duplicate_names() {
        let decl = |name: &str| CallbackDeclaration {
            name: name.to_string(),
            ..Default::default()
        };
        assert!(
            validate_declarations(&[decl("echo"), decl("http.get"), decl("http.post")]).is_ok()
        );
        for bad in [
            "", "1abc", "has-dash", "a b", "http.", ".get", "a..b", "x\0",
        ] {
            assert!(
                validate_declarations(&[decl(bad)]).is_err(),
                "{bad:?} accepted"
            );
        }
        let err = validate_declarations(&[decl("echo"), decl("echo")]).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn dispatch_unknown_request_id_returns_false() {
        let pending: PendingCallbacks = Arc::new(DashMap::new());
        let dispatched =
            dispatch_callback_response(&pending, "unknown-id", CallbackResult::Ok("{}".into()));
        assert!(!dispatched);
    }
}
