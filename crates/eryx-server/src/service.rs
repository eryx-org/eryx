//! gRPC service implementation for the Eryx Execute RPC.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use eryx::callback_handler::run_callback_handler;
use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
use eryx::{
    Callback, CallbackRequest, OutputRequest, PythonStateSnapshot, ResourceLimits, SandboxPool,
    SecretConfig, SessionExecutor, TraceRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::Instrument;

use crate::callbacks::{self, CallbackResult, PendingCallbacks};
use crate::output::GrpcOutputHandler;
use crate::proto::eryx::v1::{
    ClientMessage, ExecuteResult, ExecuteStats, ServerMessage, SupportingFile, callback_response,
    client_message, server_message,
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
        let persist_state = execute_req.persist_state;
        let state_snapshot = execute_req.state_snapshot;
        let files = execute_req.files;

        let span = tracing::info_span!(
            "execute",
            code_len = code.len(),
            callbacks = declarations.len(),
            tracing = enable_tracing,
            persist_state,
            supporting_files = files.len(),
        );

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

        span.in_scope(|| {
            tracing::info!(
                timeout = ?resource_limits.execution_timeout,
                max_memory_bytes = ?resource_limits.max_memory_bytes,
                max_fuel = ?resource_limits.max_fuel,
                "request received"
            );
        });

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
        let acquire_start = Instant::now();
        let mut sandbox = self
            .pool
            .acquire()
            .await
            .map_err(|e| Status::unavailable(format!("failed to acquire sandbox: {e}")))?
            .with_callbacks(cbs)
            .with_output_handler(GrpcOutputHandler::new(server_tx.clone()))
            .with_resource_limits(resource_limits.clone());
        span.in_scope(|| {
            tracing::info!(
                acquire_ms = acquire_start.elapsed().as_millis() as u64,
                "sandbox acquired from pool"
            );
        });

        // Conditionally add trace handler.
        if enable_tracing {
            sandbox = sandbox.with_trace_handler(GrpcTraceHandler::new(server_tx.clone()));
        }

        // 6. Spawn callback dispatch task: reads inbound CallbackResponses and
        //    routes them to the pending oneshot senders.
        let pending_dispatch = Arc::clone(&pending);
        tokio::spawn(
            async move {
                while let Ok(Some(msg)) = inbound.message().await {
                    if let Some(client_message::Message::CallbackResponse(resp)) = msg.message {
                        let request_id = resp.request_id;
                        let result = match resp.result {
                            Some(callback_response::Result::JsonResult(json)) => {
                                tracing::debug!(%request_id, "dispatching callback result (ok)");
                                CallbackResult::Ok(json)
                            }
                            Some(callback_response::Result::Error(err)) => {
                                tracing::debug!(%request_id, "dispatching callback result (error)");
                                CallbackResult::Err(err)
                            }
                            None => {
                                tracing::debug!(%request_id, "dispatching callback result (empty)");
                                CallbackResult::Err("empty callback response".to_string())
                            }
                        };
                        callbacks::dispatch_callback_response(
                            &pending_dispatch,
                            &request_id,
                            result,
                        );
                    }
                }
            }
            .instrument(span.clone()),
        );

        // 7. Spawn execution task.
        let server_tx_result = server_tx;
        let resp_tx_final = resp_tx;
        tokio::spawn(
            async move {
                let result_msg = if persist_state {
                    execute_with_session(
                        &mut sandbox,
                        &code,
                        &files,
                        &state_snapshot,
                        enable_tracing,
                        &server_tx_result,
                        resource_limits,
                    )
                    .await
                } else {
                    if !files.is_empty() {
                        tracing::warn!(
                            file_count = files.len(),
                            "supporting files ignored in stateless mode (requires persist_state=true)"
                        );
                    }
                    execute_stateless(&sandbox, &code).await
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
            }
            .instrument(span.clone()),
        );

        Ok(Response::new(ReceiverStream::new(resp_rx)))
    }
}

/// Execute Python code statelessly via the sandbox's built-in execute method.
async fn execute_stateless(sandbox: &eryx::PooledSandbox, code: &str) -> ExecuteResult {
    match sandbox.execute(code).await {
        Ok(result) => {
            tracing::info!(
                success = true,
                duration_ms = result.stats.duration.as_millis() as u64,
                callback_invocations = result.stats.callback_invocations,
                "execution completed"
            );
            ExecuteResult {
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
                state_snapshot: Vec::new(),
            }
        }
        Err(e) => {
            tracing::warn!(success = false, error = %e, "execution completed");
            ExecuteResult {
                success: false,
                stdout: String::new(),
                stderr: String::new(),
                error: e.to_string(),
                stats: None,
                state_snapshot: Vec::new(),
            }
        }
    }
}

/// Create a VFS storage pre-populated with the given supporting files.
///
/// Files are written to `/data/<name>` so they can be imported as Python
/// modules once `/data` is added to `sys.path`.
async fn create_vfs_with_files(files: &[SupportingFile]) -> Arc<ArcStorage> {
    let storage = Arc::new(ArcStorage::new(
        Arc::new(InMemoryStorage::new()) as Arc<dyn VfsStorage>
    ));

    // The VFS mount path is /data (the default). The InMemoryStorage root is /,
    // so files at /data/<name> in the guest map to /<name> in storage.
    for f in files {
        let path = format!("/{}", f.name);
        if let Err(e) = storage.write(&path, f.content.as_bytes()).await {
            tracing::warn!(file = %f.name, error = %e, "failed to write supporting file to VFS");
        } else {
            tracing::debug!(file = %f.name, size = f.content.len(), "wrote supporting file to VFS");
        }
    }

    storage
}

/// Execute Python code with session-based state persistence.
///
/// Creates a [`SessionExecutor`] from the sandbox's Python executor, optionally
/// restores a previous state snapshot, executes the code, and captures a new
/// snapshot for the caller to persist.
async fn execute_with_session(
    sandbox: &mut eryx::PooledSandbox,
    code: &str,
    files: &[SupportingFile],
    state_snapshot: &[u8],
    enable_tracing: bool,
    server_tx: &mpsc::Sender<ServerMessage>,
    resource_limits: ResourceLimits,
) -> ExecuteResult {
    let start = Instant::now();

    // Get the shared PythonExecutor and clone the configured callbacks from the sandbox.
    let executor = sandbox.executor();
    let callbacks_map: HashMap<String, Arc<dyn Callback>> = sandbox
        .callbacks()
        .iter()
        .map(|(k, v)| (k.clone(), Arc::clone(v)))
        .collect();
    let callbacks_arc: Vec<Arc<dyn Callback>> = callbacks_map.values().cloned().collect();

    // Create the session executor, optionally with pre-populated VFS for supporting files.
    let mut session = if files.is_empty() {
        match SessionExecutor::new(executor, &callbacks_arc).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to create session executor");
                return ExecuteResult {
                    success: false,
                    error: format!("session creation failed: {e}"),
                    ..Default::default()
                };
            }
        }
    } else {
        let vfs_storage = create_vfs_with_files(files).await;
        tracing::info!(
            file_count = files.len(),
            "created VFS with supporting files"
        );
        match SessionExecutor::new_with_vfs(executor, &callbacks_arc, vfs_storage).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to create session executor with VFS");
                return ExecuteResult {
                    success: false,
                    error: format!("session creation failed: {e}"),
                    ..Default::default()
                };
            }
        }
    };

    // Restore previous state if provided.
    if !state_snapshot.is_empty() {
        match PythonStateSnapshot::from_bytes(state_snapshot) {
            Ok(snapshot) => {
                if let Err(e) = session.restore_state(&snapshot).await {
                    tracing::warn!(error = %e, "failed to restore state, proceeding with clean state");
                    // Don't fail — just proceed with a fresh session.
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "invalid state snapshot bytes, proceeding with clean state");
            }
        }
    }

    // If we have supporting files, run a setup step (without tracing) to add
    // /data to sys.path so the user's code can `import` the modules directly.
    if !files.is_empty() {
        let setup_code = "import sys\nif '/data' not in sys.path: sys.path.insert(0, '/data')";
        if let Err(e) = session.execute(setup_code).run().await {
            tracing::warn!(error = %e, "failed to set up sys.path for supporting files");
            // Non-fatal — user code may still work if it doesn't import the files.
        }
    }

    // Set up callback handler channels (mirroring Sandbox::execute pattern).
    let (callback_tx, callback_rx) = mpsc::channel::<CallbackRequest>(32);
    let cb_map = Arc::new(callbacks_map);
    let cb_secrets: Arc<HashMap<String, SecretConfig>> = Arc::new(HashMap::new());
    let callback_handler = tokio::spawn(async move {
        run_callback_handler(callback_rx, cb_map, resource_limits, cb_secrets).await
    });

    // Set up trace channel if tracing is enabled.
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel::<TraceRequest>();
    if enable_tracing {
        let trace_server_tx = server_tx.clone();
        tokio::spawn(async move {
            while let Some(req) = trace_rx.recv().await {
                // Parse event_json into a proto TraceEvent.
                let (event_type, function, message, name, duration_ms) =
                    parse_trace_event_json(&req.event_json);
                let proto_event = crate::proto::eryx::v1::TraceEvent {
                    lineno: req.lineno,
                    event_type,
                    function,
                    message,
                    name,
                    duration_ms,
                    context_json: req.context_json,
                };
                let msg = ServerMessage {
                    message: Some(server_message::Message::TraceEvent(proto_event)),
                };
                if trace_server_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
    }

    // Set up output streaming channel.
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputRequest>();
    let output_server_tx = server_tx.clone();
    tokio::spawn(async move {
        use crate::proto::eryx::v1::{OutputEvent, OutputStream};
        while let Some(req) = output_rx.recv().await {
            let stream = if req.stream == 0 {
                OutputStream::Stdout
            } else {
                OutputStream::Stderr
            };
            let msg = ServerMessage {
                message: Some(server_message::Message::OutputEvent(OutputEvent {
                    stream: stream.into(),
                    data: req.data,
                })),
            };
            if output_server_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Execute the user code with the session.
    let mut builder = session
        .execute(code)
        .with_callbacks(&callbacks_arc, callback_tx)
        .with_output_streaming(output_tx);
    if enable_tracing {
        builder = builder.with_tracing(trace_tx);
    }
    let exec_result = builder.run().await;

    // Wait for callback handler to finish.
    let callback_invocations = callback_handler.await.unwrap_or(0);

    // Snapshot state after execution.
    let snapshot_bytes = match session.snapshot_state().await {
        Ok(snapshot) => {
            let bytes = snapshot.to_bytes();
            tracing::info!(snapshot_bytes = bytes.len(), "state snapshot captured");
            bytes
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to capture state snapshot");
            Vec::new()
        }
    };

    let duration = start.elapsed();

    match exec_result {
        Ok(output) => {
            tracing::info!(
                success = true,
                duration_ms = duration.as_millis() as u64,
                callback_invocations,
                snapshot_bytes = snapshot_bytes.len(),
                "session execution completed"
            );
            ExecuteResult {
                success: true,
                stdout: output.stdout,
                stderr: output.stderr,
                error: String::new(),
                stats: Some(ExecuteStats {
                    duration_ms: duration.as_millis() as u64,
                    callback_invocations,
                    peak_memory_bytes: output.peak_memory_bytes,
                    fuel_consumed: output.fuel_consumed.unwrap_or(0),
                }),
                state_snapshot: snapshot_bytes,
            }
        }
        Err(e) => {
            tracing::warn!(
                success = false,
                error = %e,
                "session execution completed"
            );
            ExecuteResult {
                success: false,
                stdout: String::new(),
                stderr: String::new(),
                error: e.to_string(),
                stats: None,
                // Still return the snapshot — the Go service decides whether to keep it.
                state_snapshot: snapshot_bytes,
            }
        }
    }
}

/// Parse a trace event JSON string into its proto components.
///
/// The event_json from eryx's TraceRequest contains a JSON representation of the
/// trace event type. We extract the relevant fields for the proto message.
fn parse_trace_event_json(event_json: &str) -> (String, String, String, String, u64) {
    // event_json is typically: "line", {"call": {"function": "foo"}}, etc.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(event_json) {
        match &value {
            serde_json::Value::String(s) => {
                (s.clone(), String::new(), String::new(), String::new(), 0)
            }
            serde_json::Value::Object(map) => {
                if let Some(inner) = map.get("call") {
                    let function = inner
                        .get("function")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (
                        "call".to_string(),
                        function,
                        String::new(),
                        String::new(),
                        0,
                    )
                } else if let Some(inner) = map.get("return") {
                    let function = inner
                        .get("function")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (
                        "return".to_string(),
                        function,
                        String::new(),
                        String::new(),
                        0,
                    )
                } else if let Some(inner) = map.get("exception") {
                    let message = inner
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (
                        "exception".to_string(),
                        String::new(),
                        message,
                        String::new(),
                        0,
                    )
                } else if let Some(inner) = map.get("callback_start") {
                    let name = inner
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    (
                        "callback_start".to_string(),
                        String::new(),
                        String::new(),
                        name,
                        0,
                    )
                } else if let Some(inner) = map.get("callback_end") {
                    let name = inner
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let duration_ms = inner
                        .get("duration_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    (
                        "callback_end".to_string(),
                        String::new(),
                        String::new(),
                        name,
                        duration_ms,
                    )
                } else {
                    (
                        "unknown".to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        0,
                    )
                }
            }
            _ => (
                "unknown".to_string(),
                String::new(),
                String::new(),
                String::new(),
                0,
            ),
        }
    } else {
        (
            event_json.to_string(),
            String::new(),
            String::new(),
            String::new(),
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_vfs_with_files() {
        let files = vec![
            SupportingFile {
                name: "helpers.py".to_string(),
                content: "def greet(): return 'hello'".to_string(),
            },
            SupportingFile {
                name: "utils.py".to_string(),
                content: "PI = 3.14".to_string(),
            },
        ];

        let storage = create_vfs_with_files(&files).await;

        // Files are stored at /<name> in VFS storage (the /data mount maps / in storage).
        let content = storage.read("/helpers.py").await.unwrap();
        assert_eq!(
            String::from_utf8(content).unwrap(),
            "def greet(): return 'hello'"
        );

        let content = storage.read("/utils.py").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "PI = 3.14");
    }

    #[tokio::test]
    async fn test_create_vfs_with_no_files() {
        let storage = create_vfs_with_files(&[]).await;
        // Should just be an empty storage with root dir
        assert!(storage.read("/helpers.py").await.is_err());
    }
}
