//! gRPC service implementation for the Eryx Execute RPC.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use eryx::{ResourceLimits, SandboxPool};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::callbacks::{self, CallbackResult, PendingCallbacks};
use crate::output::GrpcOutputHandler;
use crate::proto::eryx::v1::{
    ClientMessage, ExecuteResult, ExecuteStats, ServerMessage, callback_response, client_message,
    server_message,
};
use crate::trace::GrpcTraceHandler;

/// The Eryx gRPC service.
///
/// Holds a shared [`SandboxPool`] for efficient sandbox reuse across requests.
#[derive(Debug)]
pub struct EryxService {
    pool: Arc<SandboxPool>,
}

impl EryxService {
    /// Create a new service instance backed by the given sandbox pool.
    #[must_use]
    pub fn new(pool: Arc<SandboxPool>) -> Self {
        Self { pool }
    }
}

#[tonic::async_trait]
impl crate::proto::eryx::v1::eryx_server::Eryx for EryxService {
    type ExecuteStream = ReceiverStream<Result<ServerMessage, Status>>;

    async fn execute(
        &self,
        request: Request<Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let mut inbound = request.into_inner();

        // 1. Read the first message — must be ExecuteRequest.
        let first_msg = inbound
            .message()
            .await
            .map_err(|e| Status::internal(format!("failed to read first message: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before ExecuteRequest"))?;

        let execute_req = match first_msg.message {
            Some(client_message::Message::ExecuteRequest(req)) => req,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be ExecuteRequest",
                ));
            }
        };

        let code = execute_req.code;
        let declarations = execute_req.callbacks;
        let enable_tracing = execute_req.enable_tracing;

        // 2. Parse resource limits.
        let resource_limits = if let Some(limits) = execute_req.resource_limits {
            ResourceLimits {
                execution_timeout: if limits.execution_timeout_ms > 0 {
                    Some(Duration::from_millis(limits.execution_timeout_ms))
                } else {
                    ResourceLimits::default().execution_timeout
                },
                callback_timeout: if limits.callback_timeout_ms > 0 {
                    Some(Duration::from_millis(limits.callback_timeout_ms))
                } else {
                    ResourceLimits::default().callback_timeout
                },
                max_memory_bytes: if limits.max_memory_bytes > 0 {
                    Some(limits.max_memory_bytes)
                } else {
                    ResourceLimits::default().max_memory_bytes
                },
                max_callback_invocations: if limits.max_callback_invocations > 0 {
                    Some(limits.max_callback_invocations)
                } else {
                    ResourceLimits::default().max_callback_invocations
                },
                max_fuel: if limits.max_fuel > 0 {
                    Some(limits.max_fuel)
                } else {
                    None
                },
            }
        } else {
            ResourceLimits::default()
        };

        // 3. Set up gRPC response channel.
        let (resp_tx, resp_rx) = mpsc::channel::<Result<ServerMessage, Status>>(64);

        // Internal channel for callbacks/output/trace (no Result wrapper).
        let (server_tx, mut server_rx) = mpsc::channel::<ServerMessage>(64);

        // Forward internal messages to the gRPC response stream.
        let resp_tx_fwd = resp_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = server_rx.recv().await {
                if resp_tx_fwd.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        // 4. Build callbacks from declarations.
        let pending: PendingCallbacks = Arc::new(DashMap::new());
        let cbs =
            callbacks::build_callbacks(&declarations, server_tx.clone(), Arc::clone(&pending));

        // 5. Acquire sandbox from pool and configure for this request.
        let mut sandbox = self
            .pool
            .acquire()
            .await
            .map_err(|e| Status::unavailable(format!("failed to acquire sandbox: {e}")))?
            .with_callbacks(cbs)
            .with_output_handler(GrpcOutputHandler::new(server_tx.clone()))
            .with_resource_limits(resource_limits);

        // Conditionally add trace handler.
        if enable_tracing {
            sandbox = sandbox.with_trace_handler(GrpcTraceHandler::new(server_tx.clone()));
        }

        // 6. Spawn callback dispatch task: reads inbound CallbackResponses and
        //    routes them to the pending oneshot senders.
        let pending_dispatch = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if let Some(client_message::Message::CallbackResponse(resp)) = msg.message {
                    let result = match resp.result {
                        Some(callback_response::Result::JsonResult(json)) => {
                            CallbackResult::Ok(json)
                        }
                        Some(callback_response::Result::Error(err)) => CallbackResult::Err(err),
                        None => CallbackResult::Err("empty callback response".to_string()),
                    };
                    callbacks::dispatch_callback_response(
                        &pending_dispatch,
                        &resp.request_id,
                        result,
                    );
                }
            }
        });

        // 7. Spawn execution task.
        let server_tx_result = server_tx;
        let resp_tx_final = resp_tx;
        tokio::spawn(async move {
            let exec_result = sandbox.execute(&code).await;

            let result_msg = match exec_result {
                Ok(result) => ExecuteResult {
                    success: true,
                    stdout: result.stdout,
                    stderr: result.stderr,
                    error: String::new(),
                    stats: Some(ExecuteStats {
                        duration_ms: result.stats.duration.as_millis() as u64,
                        callback_invocations: result.stats.callback_invocations,
                        peak_memory_bytes: result.stats.peak_memory_bytes.unwrap_or(0),
                        fuel_consumed: result.stats.fuel_consumed.unwrap_or(0),
                    }),
                },
                Err(e) => ExecuteResult {
                    success: false,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: e.to_string(),
                    stats: None,
                },
            };

            let msg = ServerMessage {
                message: Some(server_message::Message::ExecuteResult(result_msg)),
            };

            // Send via internal channel first, then drop it so the forwarder ends.
            let _ = server_tx_result.send(msg).await;
            drop(server_tx_result);

            // sandbox is dropped here — per-request state cleared, returned to pool.
            drop(sandbox);

            // The resp_tx_final is kept alive to ensure the response stream stays
            // open until this task completes. Dropping it signals stream end.
            drop(resp_tx_final);
        });

        Ok(Response::new(ReceiverStream::new(resp_rx)))
    }
}
