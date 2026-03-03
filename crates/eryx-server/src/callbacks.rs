//! Callback bridge between gRPC and the eryx sandbox.
//!
//! Converts protobuf [`CallbackDeclaration`]s into eryx [`DynamicCallback`]s
//! that send callback requests over the gRPC response stream and await
//! responses from the client via per-request oneshot channels.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use eryx::{CallbackError, DynamicCallback};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::proto::eryx::v1::{CallbackDeclaration, CallbackRequest, ServerMessage, server_message};

/// Result of a callback invocation returned by the Go client.
#[derive(Debug)]
pub enum CallbackResult {
    /// Successful JSON result.
    Ok(String),
    /// Error message.
    Err(String),
}

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
pub fn build_callbacks(
    declarations: &[CallbackDeclaration],
    grpc_tx: mpsc::Sender<ServerMessage>,
    pending: PendingCallbacks,
) -> Vec<Box<dyn eryx::Callback>> {
    tracing::debug!(callback_count = declarations.len(), "building callbacks");
    declarations
        .iter()
        .map(|decl| {
            let tx = grpc_tx.clone();
            let pending = Arc::clone(&pending);
            let name = decl.name.clone();

            let mut builder = DynamicCallback::builder(
                decl.name.clone(),
                decl.description.clone(),
                move |args| {
                    let tx = tx.clone();
                    let pending = Arc::clone(&pending);
                    let name = name.clone();

                    Box::pin(async move {
                        let started = Instant::now();
                        let request_id = Uuid::new_v4().to_string();
                        let arguments_json = serde_json::to_string(&args)
                            .map_err(|e| CallbackError::ExecutionFailed(e.to_string()))?;

                        tracing::debug!(
                            callback_name = %name,
                            %request_id,
                            args_len = arguments_json.len(),
                            "callback invoked"
                        );

                        // Create oneshot channel for the response.
                        let (resp_tx, resp_rx) = oneshot::channel();
                        pending.insert(request_id.clone(), resp_tx);

                        // Send the callback request to the Go client.
                        let msg = ServerMessage {
                            message: Some(server_message::Message::CallbackRequest(
                                CallbackRequest {
                                    request_id: request_id.clone(),
                                    name: name.clone(),
                                    arguments_json,
                                },
                            )),
                        };

                        tx.send(msg).await.map_err(|e| {
                            tracing::warn!(
                                callback_name = %name,
                                %request_id,
                                error = %e,
                                "gRPC response channel closed"
                            );
                            CallbackError::ExecutionFailed(
                                "gRPC response channel closed".to_string(),
                            )
                        })?;

                        // Wait for the Go client to respond.
                        let result = resp_rx.await.map_err(|e| {
                            tracing::warn!(
                                callback_name = %name,
                                %request_id,
                                error = %e,
                                "callback response channel dropped"
                            );
                            CallbackError::ExecutionFailed(
                                "callback response channel dropped".to_string(),
                            )
                        })?;

                        let duration_ms = started.elapsed().as_millis() as u64;

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

/// Dispatch a [`CallbackResponse`] to its pending oneshot sender.
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

        let callbacks = build_callbacks(&decls, tx, pending);
        assert_eq!(callbacks.len(), 2);
        assert_eq!(callbacks[0].name(), "echo");
        assert_eq!(callbacks[1].name(), "noop");
    }

    #[tokio::test]
    async fn callback_sends_request_and_receives_response() {
        let (tx, mut rx) = mpsc::channel(16);
        let pending: PendingCallbacks = Arc::new(DashMap::new());

        let callbacks = build_callbacks(&[make_echo_declaration()], tx, Arc::clone(&pending));
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

        let callbacks = build_callbacks(&[make_echo_declaration()], tx, Arc::clone(&pending));
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

    #[test]
    fn dispatch_unknown_request_id_returns_false() {
        let pending: PendingCallbacks = Arc::new(DashMap::new());
        let dispatched =
            dispatch_callback_response(&pending, "unknown-id", CallbackResult::Ok("{}".into()));
        assert!(!dispatched);
    }
}
