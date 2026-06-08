//! gRPC service implementation for the Eryx Execute RPC.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use eryx::callback_handler::{run_callback_handler, run_net_handler};
use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
use eryx::{
    Callback, CallbackJournal, CallbackRequest, ConnectionManager, Error, NetConfig, NetRequest,
    OutputRequest, PythonStateSnapshot, ReplayState, ResourceLimits, SandboxPool, SecretConfig,
    SessionExecutor, TraceRequest, VfsConfig, generate_placeholder, scrub_placeholders,
};
use opentelemetry::propagation::Extractor;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status, Streaming};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::callbacks::{self, CallbackResult, PendingCallbacks};
use crate::proto::eryx::v1::{
    CallbackOutcome, ClientMessage, ExecuteResult, ExecuteStats, FailureKind, FileKind,
    ServerMessage, SupportingFile, callback_response, client_message, server_message,
};

/// Adapts tonic's [`MetadataMap`] to OpenTelemetry's [`Extractor`] trait,
/// allowing the propagator to read `traceparent`/`tracestate` from gRPC metadata.
struct MetadataExtractor<'a>(&'a MetadataMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|k| match k {
                tonic::metadata::KeyRef::Ascii(key) => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The Eryx gRPC service.
///
/// Holds a shared [`SandboxPool`] for efficient sandbox reuse across requests.
#[derive(Debug)]
pub struct EryxService {
    pool: Arc<SandboxPool>,
    journal_signer: crate::replay::JournalSigner,
}

impl EryxService {
    /// Create a new service instance with the given pool and journal signer.
    ///
    /// The `signer` should use a stable key shared across all replicas (see
    /// [`JournalSigner::from_key`](crate::replay::JournalSigner::from_key))
    /// so journals are portable. Use [`Self::new`] for tests/dev where a random
    /// ephemeral key is acceptable.
    #[must_use]
    pub fn with_signer(pool: Arc<SandboxPool>, signer: crate::replay::JournalSigner) -> Self {
        Self {
            pool,
            journal_signer: signer,
        }
    }

    /// Create a new service with a random ephemeral signing key.
    ///
    /// Journals signed by this instance cannot be verified by other replicas or
    /// after a restart. Intended for tests and single-instance dev servers.
    #[must_use]
    pub fn new(pool: Arc<SandboxPool>) -> Self {
        Self::with_signer(pool, crate::replay::JournalSigner::random())
    }
}

#[tonic::async_trait]
impl crate::proto::eryx::v1::eryx_server::Eryx for EryxService {
    type ExecuteStream = ReceiverStream<Result<ServerMessage, Status>>;

    async fn execute(
        &self,
        request: Request<Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        // Extract W3C trace context from incoming gRPC metadata so that spans
        // created here become children of the caller's trace.
        let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&MetadataExtractor(request.metadata()))
        });

        let mut inbound = request.into_inner();

        // 1. Read the first message — must be ExecuteRequest.
        let first_msg = inbound
            .message()
            .await
            .map_err(|e| Status::internal(format!("failed to read first message: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before ExecuteRequest"))?;

        let execute_req = match first_msg.message {
            // The variant is boxed (see build.rs `.boxed(...)`); deref to own the
            // request so individual fields can be moved out below.
            Some(client_message::Message::ExecuteRequest(req)) => *req,
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
        let result_variable = execute_req.result_variable;
        let scrub_result = execute_req.scrub_result;

        // Callback-result replay: presence of the journal field (even empty)
        // opts this execution into journaling; its entries seed replay.
        // Non-empty journals must pass HMAC verification (bound to `code`) — a
        // failed check discards the entries (falling back to fresh journaling,
        // not an error). This also catches journals from a different script.
        let previous_journal: Option<CallbackJournal> =
            execute_req.callback_journal.as_ref().map(|j| {
                if !j.entries.is_empty() && !self.journal_signer.verify(j, &code) {
                    tracing::warn!(
                        entries = j.entries.len(),
                        "callback journal signature verification failed — ignoring entries"
                    );
                    CallbackJournal::new(&code)
                } else {
                    crate::replay::journal_from_proto(&code, j)
                }
            });

        // Parse network config: present = networking enabled, absent = disabled.
        let net_config = execute_req.network_config.map(|nc| {
            let defaults = NetConfig::default();
            NetConfig {
                allow_all_hosts: nc.allow_all_hosts,
                allowed_hosts: nc.allowed_hosts,
                blocked_hosts: if nc.blocked_hosts.is_empty() {
                    defaults.blocked_hosts
                } else {
                    nc.blocked_hosts
                },
                max_connections: if nc.max_connections > 0 {
                    nc.max_connections
                } else {
                    defaults.max_connections
                },
                connect_timeout: if nc.connect_timeout_ms > 0 {
                    Duration::from_millis(nc.connect_timeout_ms)
                } else {
                    defaults.connect_timeout
                },
                io_timeout: if nc.io_timeout_ms > 0 {
                    Duration::from_millis(nc.io_timeout_ms)
                } else {
                    defaults.io_timeout
                },
                custom_root_certs: vec![],
            }
        });

        // Parse and validate secrets: generate placeholders and build env preamble.
        let (secrets, secrets_preamble) = {
            let mut map = HashMap::new();
            let mut preamble = String::new();
            if !execute_req.secrets.is_empty() {
                preamble.push_str("import os\n");
            }
            for s in &execute_req.secrets {
                // Validate secret name: must be a valid Python env var name.
                if !is_valid_secret_name(&s.name) {
                    return Err(Status::invalid_argument(format!(
                        "invalid secret name {:?}: must match [A-Za-z_][A-Za-z0-9_]*",
                        s.name
                    )));
                }
                if map.contains_key(&s.name) {
                    return Err(Status::invalid_argument(format!(
                        "duplicate secret name {:?}",
                        s.name
                    )));
                }
                let placeholder = generate_placeholder(&s.name);
                preamble.push_str(&format!("os.environ[{:?}] = {:?}\n", s.name, placeholder));
                map.insert(
                    s.name.clone(),
                    SecretConfig {
                        real_value: s.value.clone(),
                        placeholder,
                        allowed_hosts: s.allowed_hosts.clone(),
                    },
                );
            }
            (map, preamble)
        };
        let has_secrets = !secrets.is_empty();
        let scrub_stdout = has_secrets && !execute_req.disable_stdout_scrub;
        let scrub_stderr = has_secrets && !execute_req.disable_stderr_scrub;

        let span = tracing::info_span!(
            "execute",
            code_len = code.len(),
            callbacks = declarations.len(),
            tracing = enable_tracing,
            persist_state,
            supporting_files = files.len(),
            networking = net_config.is_some(),
            secrets = secrets.len(),
        );
        if let Err(e) = span.set_parent(parent_cx) {
            tracing::debug!("failed to set parent trace context: {e}");
        }

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
        let mut sandbox = match self.pool.acquire().await {
            Ok(s) => s,
            Err(e) => {
                let stats = self.pool.stats();
                span.in_scope(|| {
                    tracing::warn!(
                        error = %e,
                        pool_in_use = stats.in_use,
                        pool_available = stats.available,
                        pool_total = stats.total,
                        "failed to acquire sandbox from pool"
                    );
                });
                return Err(Status::unavailable(format!(
                    "failed to acquire sandbox: {e}"
                )));
            }
        };
        sandbox = sandbox
            .with_callbacks(cbs)
            .with_resource_limits(resource_limits.clone());
        let pool_stats = self.pool.stats();
        span.in_scope(|| {
            tracing::info!(
                acquire_ms = acquire_start.elapsed().as_millis() as u64,
                pool_in_use = pool_stats.in_use,
                pool_available = pool_stats.available,
                pool_total = pool_stats.total,
                "sandbox acquired from pool"
            );
        });
        metrics::counter!("eryx_sandbox_acquisitions_total").increment(1);

        // Create a cancellation token so the dispatch task can signal the
        // execution task when the client disconnects.
        let cancel = CancellationToken::new();

        // 6. Spawn callback dispatch task: reads inbound CallbackResponses and
        //    routes them to the pending oneshot senders.
        let pending_dispatch = Arc::clone(&pending);
        let cancel_dispatch = cancel.clone();
        tokio::spawn(
            async move {
                while let Ok(Some(msg)) = inbound.message().await {
                    if let Some(client_message::Message::CallbackResponse(resp)) = msg.message {
                        let request_id = resp.request_id;
                        // A SUSPEND outcome takes precedence over the result oneof:
                        // the reason is carried in the `error` field. This defers
                        // execution (halts the guest) rather than surfacing an
                        // ordinary value/exception.
                        let result = if resp.outcome == CallbackOutcome::Suspend as i32 {
                            let reason = match resp.result {
                                Some(callback_response::Result::Error(err)) => err,
                                _ => String::new(),
                            };
                            tracing::debug!(%request_id, "dispatching callback result (suspend)");
                            CallbackResult::Suspend(reason)
                        } else {
                            match resp.result {
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
                            }
                        };
                        callbacks::dispatch_callback_response(
                            &pending_dispatch,
                            &request_id,
                            result,
                        );
                    }
                }
                // Client disconnected — cancel pending callbacks so their oneshot
                // receivers error immediately instead of waiting for callback_timeout.
                let remaining = pending_dispatch.len();
                if remaining > 0 {
                    tracing::warn!(
                        pending_callbacks = remaining,
                        "client disconnected with pending callbacks, cancelling"
                    );
                    pending_dispatch.clear();
                } else {
                    tracing::info!("client disconnected cleanly, no pending callbacks");
                }
                // Signal the execution task that the client is gone.
                cancel_dispatch.cancel();
            }
            .instrument(span.clone()),
        );

        // 7. Spawn execution task.
        let journal_signer = self.journal_signer.clone();
        let server_tx_result = server_tx;
        let resp_tx_final = resp_tx;
        let cancel_exec = cancel;
        let sandbox_held_start = Instant::now();
        tokio::spawn(
            async move {
                let params = SessionParams {
                    code: &code,
                    files: &files,
                    state: persist_state.then_some(state_snapshot.as_slice()),
                    enable_tracing,
                    resource_limits,
                    net_config,
                    secrets,
                    secrets_preamble,
                    scrub_stdout,
                    scrub_stderr,
                    scrub_result,
                    result_variable,
                    previous_journal,
                    journal_signer: journal_signer.clone(),
                };

                // Race execution against client cancellation. If the client
                // disconnects, abandon execution and release the sandbox promptly
                // instead of waiting for timeouts to expire.
                let result_msg = tokio::select! {
                    result = execute_with_session(&mut sandbox, &params, &server_tx_result) => result,
                    () = cancel_exec.cancelled() => {
                        tracing::warn!("client disconnected, aborting execution");
                        metrics::counter!("eryx_executions_cancelled_total").increment(1);
                        ExecuteResult {
                            success: false,
                            stdout: String::new(),
                            stderr: String::new(),
                            error: "execution cancelled: client disconnected".to_string(),
                            stats: None,
                            state_snapshot: Vec::new(),
                            result: String::new(),
                            result_error: String::new(),
                            failure_kind: FailureKind::Cancelled as i32,
                            callback_journal: None,
                            suspended: false,
                            suspended_callback: None,
                        }
                    }
                };

                let msg = ServerMessage {
                    message: Some(server_message::Message::ExecuteResult(Box::new(result_msg))),
                };

                // Send via internal channel first, then drop it so the forwarder ends.
                let _ = server_tx_result.send(msg).await;
                drop(server_tx_result);

                // sandbox is dropped here — per-request state cleared, returned to pool.
                let sandbox_held_ms = sandbox_held_start.elapsed().as_millis() as u64;
                tracing::debug!(
                    sandbox_held_ms,
                    "execution task finished, returning sandbox to pool"
                );
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

/// Builtins injection code — copies callable callbacks from globals() into the
/// builtins module so they're visible in imported modules too.
const BUILTINS_INJECT: &str = concat!(
    "import builtins as _b\n",
    "for _k, _v in list(globals().items()):\n",
    "    if not _k.startswith('_') and callable(_v):\n",
    "        setattr(_b, _k, _v)\n",
);

/// Sys-path setup code — adds `/eryx/lib` to `sys.path` and invalidates importlib caches
/// so newly-mounted VFS files are discoverable. Merged into the single user-code execution
/// to avoid a separate WASM execution cycle.
const SYS_PATH_INJECT: &str = concat!(
    "import sys as _sys, importlib as _il\n",
    "_il.invalidate_caches()\n",
    "if '/eryx/lib' not in _sys.path: _sys.path.insert(0, '/eryx/lib')\n",
    "del _sys, _il\n",
);

/// Check whether a secret name is a valid Python environment variable name.
///
/// Must match `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check whether a filename is safe for use in VFS paths.
///
/// Rejects names that contain path separators (`/`, `\`) or `..` components,
/// which could be used to escape the intended directory.
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..")
}

/// Create a VFS storage pre-populated with the given supporting files.
///
/// Files are routed into subdirectories under the VFS mount based on their kind:
/// - `MODULE` files go to `{mount_path}/lib/<name>` (added to `sys.path`)
/// - `DATA` files go to `{mount_path}/data/<name>` (readable but not importable)
///
/// The `mount_path` must match the VFS preopen path (e.g. `/eryx`) because
/// `VfsDescriptor::resolve_path` prepends it when the guest accesses files.
async fn create_vfs_with_files(files: &[SupportingFile], mount_path: &str) -> ArcStorage {
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()) as Arc<dyn VfsStorage>);

    let lib_path = format!("{mount_path}/lib");
    let data_path = format!("{mount_path}/data");

    // Create subdirectories under the mount path. The VFS preopen will also
    // create the mount_path dir itself via add_vfs_preopen, but we need the
    // sub-dirs to exist before writing files.
    let _ = storage.mkdir_sync(&lib_path);
    let _ = storage.mkdir_sync(&data_path);

    let mut mounted_count: usize = 0;
    let mut total_bytes: usize = 0;

    for f in files {
        if !is_safe_filename(&f.name) {
            tracing::warn!(file = %f.name, "skipping supporting file with unsafe name (path separators or '..' not allowed)");
            continue;
        }
        let dir = match f.kind() {
            FileKind::Module => &lib_path,
            FileKind::Data => &data_path,
        };
        let path = format!("{dir}/{}", f.name);
        if let Err(e) = storage.write(&path, f.content.as_bytes()).await {
            tracing::warn!(file = %f.name, kind = ?f.kind(), error = %e, "failed to write supporting file to VFS");
        } else {
            tracing::debug!(file = %f.name, kind = ?f.kind(), dir, size = f.content.len(), "wrote supporting file to VFS");
            mounted_count += 1;
            total_bytes += f.content.len();
        }
    }

    tracing::info!(
        mounted_files = mounted_count,
        total_bytes,
        requested_files = files.len(),
        "VFS file mounting complete"
    );

    storage
}

/// Parameters for a single session execution.
struct SessionParams<'a> {
    code: &'a str,
    files: &'a [SupportingFile],
    state: Option<&'a [u8]>,
    enable_tracing: bool,
    resource_limits: ResourceLimits,
    net_config: Option<NetConfig>,
    secrets: HashMap<String, SecretConfig>,
    secrets_preamble: String,
    scrub_stdout: bool,
    scrub_stderr: bool,
    /// Enable scrubbing of the structured result/result_error. Default OFF (opt-in).
    scrub_result: bool,
    /// Name of the variable captured as the structured result. Empty = default "result".
    result_variable: String,
    /// Previous callback journal to replay from. `Some` (even when empty) enables
    /// callback journaling/replay for this execution; `None` disables it entirely.
    previous_journal: Option<CallbackJournal>,
    /// Signer for HMAC-signing outgoing journals.
    journal_signer: crate::replay::JournalSigner,
}

/// Execute Python code using a session executor.
///
/// Creates a [`SessionExecutor`] from the sandbox's Python executor, mounts any
/// supporting files into the VFS, injects callbacks into builtins, executes the
/// code, and optionally saves/restores state snapshots.
///
/// When `state` is `Some(bytes)`, the session restores from `bytes` (if non-empty)
/// before execution and captures a snapshot afterwards. When `None`, no state
/// persistence occurs.
#[tracing::instrument(skip_all, fields(code_len = params.code.len(), enable_tracing = params.enable_tracing, has_state = params.state.is_some()))]
async fn execute_with_session(
    sandbox: &mut eryx::PooledSandbox,
    params: &SessionParams<'_>,
    server_tx: &mpsc::Sender<ServerMessage>,
) -> ExecuteResult {
    let start = Instant::now();

    // Get the shared PythonExecutor and clone the configured callbacks from the sandbox.
    let executor = sandbox.executor();
    let callbacks_ref = sandbox.callbacks();

    // Set up callback-result replay when the request opted in. The same shared
    // ReplayState is used for both the executor registration and the callback
    // handler so the replay cursor advances across all callbacks in order.
    let replay_state: Option<Arc<Mutex<ReplayState>>> = params
        .previous_journal
        .as_ref()
        .map(|journal| Arc::new(Mutex::new(ReplayState::new(journal.clone()))));
    let active_callbacks: HashMap<String, Arc<dyn Callback>> = match &replay_state {
        Some(state) => crate::replay::wrap_for_replay(callbacks_ref, state),
        None => callbacks_ref.clone(),
    };
    let callbacks_arc: Vec<Arc<dyn Callback>> = active_callbacks.values().cloned().collect();

    // Create the session executor, optionally with pre-populated VFS for supporting files.
    let mut session = if params.files.is_empty() {
        match SessionExecutor::new(executor, &callbacks_arc).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    code_len = params.code.len(),
                    file_count = params.files.len(),
                    "failed to create session executor"
                );
                return ExecuteResult {
                    success: false,
                    error: format!("session creation failed: {e}"),
                    failure_kind: FailureKind::SandboxError as i32,
                    ..Default::default()
                };
            }
        }
    } else {
        let vfs_mount_path = "/eryx";
        let vfs_storage = create_vfs_with_files(params.files, vfs_mount_path).await;
        tracing::info!(
            file_count = params.files.len(),
            "created VFS with supporting files"
        );
        let vfs_config = VfsConfig::new(vfs_mount_path);
        match SessionExecutor::new_with_vfs_config(
            executor,
            &callbacks_arc,
            vfs_storage,
            vfs_config,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    code_len = params.code.len(),
                    file_count = params.files.len(),
                    "failed to create session executor with VFS"
                );
                return ExecuteResult {
                    success: false,
                    error: format!("session creation failed: {e}"),
                    failure_kind: FailureKind::SandboxError as i32,
                    ..Default::default()
                };
            }
        }
    };

    // Apply resource limits to the session so they take effect for all executions.
    session.set_execution_timeout(params.resource_limits.execution_timeout);
    session.set_fuel_limit(params.resource_limits.max_fuel);

    // Restore previous state if provided.
    if let Some(state_snapshot) = params.state.filter(|s| !s.is_empty()) {
        let restore_start = Instant::now();
        match PythonStateSnapshot::from_bytes(state_snapshot) {
            Ok(snapshot) => {
                if let Err(e) = session.restore_state(&snapshot).await {
                    tracing::warn!(
                        error = %e,
                        restore_ms = restore_start.elapsed().as_millis() as u64,
                        snapshot_bytes = state_snapshot.len(),
                        "failed to restore state, proceeding with clean state"
                    );
                    // Don't fail — just proceed with a fresh session.
                } else {
                    tracing::info!(
                        restore_ms = restore_start.elapsed().as_millis() as u64,
                        snapshot_bytes = state_snapshot.len(),
                        "state restored from snapshot"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    snapshot_bytes = state_snapshot.len(),
                    "invalid state snapshot bytes, proceeding with clean state"
                );
            }
        }
    }

    // Apply a per-request result-variable override. Done *after* restore_state:
    // although restore_state reuses the current instance today, applying the
    // override last keeps it correct even if restore were to reset the instance.
    if !params.result_variable.is_empty()
        && let Err(e) = session.set_result_variable(&params.result_variable).await
    {
        tracing::warn!(error = %e, "failed to set result variable, using default");
    }

    // Set up callback handler channels (mirroring Sandbox::execute pattern).
    let (callback_tx, callback_rx) = mpsc::channel::<CallbackRequest>(32);
    let fuel_limit = params.resource_limits.max_fuel;
    // The handler invokes callbacks via this map, so it must hold the same
    // (possibly replay-wrapped) callbacks used for executor registration.
    let cb_map: Arc<HashMap<String, Arc<dyn Callback>>> = Arc::new(active_callbacks);
    let cb_secrets: Arc<HashMap<String, SecretConfig>> = Arc::new(params.secrets.clone());
    let resource_limits = params.resource_limits.clone();
    let callback_handler = tokio::spawn(
        async move { run_callback_handler(callback_rx, cb_map, resource_limits, cb_secrets).await }
            .instrument(tracing::Span::current()),
    );

    // Compute preamble line count so trace events can be adjusted to user code lines.
    let preamble_lines = {
        let mut preamble = params.secrets_preamble.clone();
        if !params.files.is_empty() {
            preamble.push_str(SYS_PATH_INJECT);
        }
        preamble.push_str(BUILTINS_INJECT);
        preamble.chars().filter(|&c| c == '\n').count() as u32
    };

    // Set up trace channel if tracing is enabled.
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel::<TraceRequest>();
    if params.enable_tracing {
        let trace_server_tx = server_tx.clone();
        tokio::spawn(async move {
            while let Some(req) = trace_rx.recv().await {
                // Adjust line numbers to account for injected preamble code.
                // Trace events from the preamble itself are suppressed.
                let adjusted_line = req.lineno.saturating_sub(preamble_lines);
                if adjusted_line == 0 && req.lineno > 0 {
                    continue;
                }

                let (event_type, function, message, callback_name, duration_ms) =
                    parse_trace_event_json(&req.event_json);

                let msg = ServerMessage {
                    message: Some(server_message::Message::TraceEvent(
                        crate::proto::eryx::v1::TraceEvent {
                            lineno: adjusted_line,
                            event_type,
                            function,
                            message,
                            name: callback_name,
                            duration_ms,
                            context_json: String::new(),
                        },
                    )),
                };
                if trace_server_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
    }

    // Set up output streaming channel with optional secret scrubbing.
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputRequest>();
    let output_server_tx = server_tx.clone();
    let output_scrub_stdout = params.scrub_stdout;
    let output_scrub_stderr = params.scrub_stderr;
    let output_secrets = if output_scrub_stdout || output_scrub_stderr {
        Some(params.secrets.clone())
    } else {
        None
    };
    // Accumulate the raw (pre-scrub) streamed output so the final ExecuteResult
    // can carry it. The error path otherwise loses captured output entirely
    // because no ExecutionOutput is produced; accumulating here lets the final
    // stdout/stderr fields match what was streamed (and the success path).
    let output_accumulator = tokio::spawn(async move {
        use crate::proto::eryx::v1::{OutputEvent, OutputStream};
        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        while let Some(req) = output_rx.recv().await {
            let stream = if req.stream == 0 {
                stdout_buf.push_str(&req.data);
                OutputStream::Stdout
            } else {
                stderr_buf.push_str(&req.data);
                OutputStream::Stderr
            };
            let data = if let Some(ref secrets) = output_secrets {
                let should_scrub = match req.stream {
                    0 => output_scrub_stdout,
                    _ => output_scrub_stderr,
                };
                if should_scrub {
                    scrub_placeholders(&req.data, secrets)
                } else {
                    req.data
                }
            } else {
                req.data
            };
            let msg = ServerMessage {
                message: Some(server_message::Message::OutputEvent(OutputEvent {
                    stream: stream.into(),
                    data,
                })),
            };
            if output_server_tx.send(msg).await.is_err() {
                break;
            }
        }
        (stdout_buf, stderr_buf)
    });

    // Spawn network handler if networking is enabled (mirrors Sandbox::execute pattern).
    let (net_tx, _net_handler) = if let Some(config) = params.net_config.clone() {
        let (tx, rx) = mpsc::channel::<NetRequest>(32);
        let allowed_hosts = config.allowed_hosts.len();
        let max_connections = config.max_connections;
        let manager = ConnectionManager::new(config, params.secrets.clone());
        tracing::info!(allowed_hosts, max_connections, "network handler started");
        let handler = tokio::spawn(async move {
            let result = run_net_handler(rx, manager).await;
            tracing::info!("network handler stopped");
            result
        });
        (Some(tx), Some(handler))
    } else {
        (None, None)
    };

    // Prepend builtins injection to user code so callbacks are accessible from
    // imported modules. Also prepend sys.path setup when supporting files are
    // present — merged here to avoid a separate WASM execution cycle.
    let full_code = if !params.files.is_empty() {
        format!(
            "{}{SYS_PATH_INJECT}{BUILTINS_INJECT}{}",
            params.secrets_preamble, params.code
        )
    } else {
        format!(
            "{}{BUILTINS_INJECT}{}",
            params.secrets_preamble, params.code
        )
    };
    let mut builder = session
        .execute(&full_code)
        .with_callbacks(&callbacks_arc, callback_tx)
        .with_output_streaming(output_tx);
    if let Some(fuel) = fuel_limit {
        builder = builder.with_fuel_limit(fuel);
    }
    if params.enable_tracing {
        builder = builder.with_tracing(trace_tx);
    }
    if let Some(tx) = net_tx {
        builder = builder.with_network(tx);
    }
    let exec_result = builder.run().await;

    // Wait for callback handler to finish.
    let callback_invocations = callback_handler.await.unwrap_or(0);

    // The output channel sender is dropped once execution completes, so the
    // accumulator task has now (or will shortly) finish draining. Its captured
    // stdout/stderr are used to populate the error-path result below.
    let (streamed_stdout, streamed_stderr) = output_accumulator.await.unwrap_or_default();

    // Extract the recorded replay journal. Read regardless of success/failure so
    // the caller can replay completed callbacks even when a later one errored or
    // suspended. The suspension metadata (if any) is read from the same guard.
    let (proto_journal, replayed_callbacks, suspended_callback) = match &replay_state {
        Some(state) => {
            let guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut journal = crate::replay::journal_to_proto(&guard.build_journal(params.code));
            params.journal_signer.sign(&mut journal, params.code);
            let suspended = guard.suspended().map(crate::replay::suspended_to_proto);
            (Some(journal), guard.replayed_count(), suspended)
        }
        None => (None, 0, None),
    };
    let suspended = suspended_callback.is_some();

    // Snapshot state after execution (only when persistence is requested).
    let snapshot_bytes = if params.state.is_some() {
        let snapshot_start = Instant::now();
        match session.snapshot_state().await {
            Ok(snapshot) => {
                let bytes = snapshot.to_bytes();
                tracing::info!(
                    snapshot_bytes = bytes.len(),
                    snapshot_ms = snapshot_start.elapsed().as_millis() as u64,
                    "state snapshot captured"
                );
                bytes
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    snapshot_ms = snapshot_start.elapsed().as_millis() as u64,
                    "failed to capture state snapshot"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
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
            metrics::counter!("eryx_executions_total", "status" => "success").increment(1);
            metrics::histogram!("eryx_execution_duration_seconds").record(duration.as_secs_f64());
            let stdout = if params.scrub_stdout {
                scrub_placeholders(&output.stdout, &params.secrets)
            } else {
                output.stdout
            };
            let stderr = if params.scrub_stderr {
                scrub_placeholders(&output.stderr, &params.secrets)
            } else {
                output.stderr
            };
            // The structured result is a programmatic side channel, so scrubbing is
            // opt-in (params.scrub_result), independent of the stdout/stderr policy.
            // When enabled, scrub both the result and its error message.
            let (result, result_error) = if params.scrub_result {
                (
                    output
                        .result
                        .map(|r| scrub_placeholders(&r, &params.secrets))
                        .unwrap_or_default(),
                    output
                        .result_error
                        .map(|e| scrub_placeholders(&e, &params.secrets))
                        .unwrap_or_default(),
                )
            } else {
                (
                    output.result.unwrap_or_default(),
                    output.result_error.unwrap_or_default(),
                )
            };
            ExecuteResult {
                success: true,
                stdout,
                stderr,
                error: String::new(),
                stats: Some(ExecuteStats {
                    duration_ms: duration.as_millis() as u64,
                    callback_invocations,
                    peak_memory_bytes: output.peak_memory_bytes,
                    fuel_consumed: output.fuel_consumed.unwrap_or(0),
                    replayed_callbacks,
                }),
                state_snapshot: snapshot_bytes,
                result,
                result_error,
                failure_kind: FailureKind::Unspecified as i32,
                callback_journal: proto_journal,
                suspended,
                suspended_callback,
            }
        }
        Err(e) => {
            let failure_kind = classify_failure(&e);
            tracing::warn!(
                success = false,
                error = %e,
                failure_kind = ?failure_kind,
                "session execution completed"
            );
            metrics::counter!("eryx_executions_total", "status" => "error").increment(1);
            metrics::histogram!("eryx_execution_duration_seconds").record(duration.as_secs_f64());
            // Surface output captured before the failure (e.g. an uncaught
            // exception's traceback, which Python writes to stderr) so the
            // stdout/stderr fields match what was streamed and the success path.
            // Scrub the whole buffer, consistent with the Ok arm.
            let stdout = if params.scrub_stdout {
                scrub_placeholders(&streamed_stdout, &params.secrets)
            } else {
                streamed_stdout
            };
            let stderr = if params.scrub_stderr {
                scrub_placeholders(&streamed_stderr, &params.secrets)
            } else {
                streamed_stderr
            };
            // For a script exception the traceback is already in `stderr`
            // (CPython-style); `error` is reserved for sandbox failures, so leave
            // it empty and let callers branch on `failure_kind`. For machinery
            // failures, scrub the message as defense-in-depth in case a future
            // error string ever interpolates user-derived input. (Secrets reach
            // Python only as placeholders, so this scrubs placeholders, never
            // real values; it is a no-op when no secrets are configured.)
            let error = if failure_kind == FailureKind::ScriptException {
                String::new()
            } else {
                scrub_placeholders(&e.to_string(), &params.secrets)
            };
            ExecuteResult {
                success: false,
                stdout,
                stderr,
                error,
                // Provide stats only when replay is active (to surface
                // replayed_callbacks); otherwise keep the previous behavior of
                // omitting stats on error.
                stats: replay_state.is_some().then_some(ExecuteStats {
                    duration_ms: duration.as_millis() as u64,
                    callback_invocations,
                    // peak_memory_bytes and fuel_consumed come from the
                    // execution `output`, which we don't have on the error path;
                    // they are intentionally 0 here (unavailable), unlike the
                    // success path which populates them.
                    peak_memory_bytes: 0,
                    fuel_consumed: 0,
                    replayed_callbacks,
                }),
                // Still return the snapshot — the Go service decides whether to keep it.
                state_snapshot: snapshot_bytes,
                result: String::new(),
                result_error: String::new(),
                failure_kind: failure_kind as i32,
                callback_journal: proto_journal,
                suspended,
                suspended_callback,
            }
        }
    }
}

/// Classify a library [`Error`] into the proto [`FailureKind`].
///
/// [`Error::PythonException`] is a *script* failure — its traceback is surfaced
/// in `stderr` — so it maps to [`FailureKind::ScriptException`]. Everything else
/// is a sandbox-level failure whose message belongs in the `error` field.
fn classify_failure(error: &Error) -> FailureKind {
    match error {
        Error::PythonException(_) => FailureKind::ScriptException,
        Error::Timeout(_) => FailureKind::Timeout,
        Error::FuelExhausted { .. } => FailureKind::FuelExhausted,
        Error::Cancelled => FailureKind::Cancelled,
        _ => FailureKind::SandboxError,
    }
}

/// Parse a trace event JSON string into its proto components.
///
/// The event_json uses serde's internally-tagged format with a `"type"` key:
/// `{"type":"line"}`, `{"type":"call","function":"foo"}`, etc.
fn parse_trace_event_json(event_json: &str) -> (String, String, String, String, u64) {
    let str_or = |map: &serde_json::Map<String, serde_json::Value>, key: &str| -> String {
        map.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(event_json)
    else {
        return (
            event_json.to_string(),
            String::new(),
            String::new(),
            String::new(),
            0,
        );
    };

    let event_type = str_or(&map, "type");
    match event_type.as_str() {
        "line" => (
            "line".into(),
            String::new(),
            String::new(),
            String::new(),
            0,
        ),
        "call" => (
            "call".into(),
            str_or(&map, "function"),
            String::new(),
            String::new(),
            0,
        ),
        "return" => (
            "return".into(),
            str_or(&map, "function"),
            String::new(),
            String::new(),
            0,
        ),
        "exception" => (
            "exception".into(),
            String::new(),
            str_or(&map, "message"),
            String::new(),
            0,
        ),
        "callback_start" => (
            "callback_start".into(),
            String::new(),
            String::new(),
            str_or(&map, "name"),
            0,
        ),
        "callback_end" => {
            let duration_ms = map.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            (
                "callback_end".into(),
                String::new(),
                String::new(),
                str_or(&map, "name"),
                duration_ms,
            )
        }
        _ => (
            "unknown".into(),
            String::new(),
            String::new(),
            String::new(),
            0,
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MOUNT_PATH: &str = "/eryx";

    #[tokio::test]
    async fn test_create_vfs_with_files() {
        let files = vec![
            SupportingFile {
                name: "helpers.py".to_string(),
                content: "def greet(): return 'hello'".to_string(),
                kind: FileKind::Module as i32,
            },
            SupportingFile {
                name: "utils.py".to_string(),
                content: "PI = 3.14".to_string(),
                kind: FileKind::Module as i32,
            },
        ];

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await;

        // MODULE files are stored at {mount_path}/lib/<name> in VFS storage.
        let content = storage.read("/eryx/lib/helpers.py").await.unwrap();
        assert_eq!(
            String::from_utf8(content).unwrap(),
            "def greet(): return 'hello'"
        );

        let content = storage.read("/eryx/lib/utils.py").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "PI = 3.14");
    }

    #[tokio::test]
    async fn test_create_vfs_with_data_files() {
        let files = vec![
            SupportingFile {
                name: "config.json".to_string(),
                content: r#"{"key": "value"}"#.to_string(),
                kind: FileKind::Data as i32,
            },
            SupportingFile {
                name: "input.csv".to_string(),
                content: "a,b,c\n1,2,3".to_string(),
                kind: FileKind::Data as i32,
            },
        ];

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await;

        // DATA files are stored at {mount_path}/data/<name> in VFS storage.
        let content = storage.read("/eryx/data/config.json").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), r#"{"key": "value"}"#);

        let content = storage.read("/eryx/data/input.csv").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "a,b,c\n1,2,3");
    }

    #[tokio::test]
    async fn test_create_vfs_mixed_kinds() {
        let files = vec![
            SupportingFile {
                name: "helpers.py".to_string(),
                content: "def greet(): return 'hello'".to_string(),
                kind: FileKind::Module as i32,
            },
            SupportingFile {
                name: "data.json".to_string(),
                content: r#"{"items": []}"#.to_string(),
                kind: FileKind::Data as i32,
            },
        ];

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await;

        // MODULE file goes to {mount_path}/lib/
        let content = storage.read("/eryx/lib/helpers.py").await.unwrap();
        assert_eq!(
            String::from_utf8(content).unwrap(),
            "def greet(): return 'hello'"
        );

        // DATA file goes to {mount_path}/data/
        let content = storage.read("/eryx/data/data.json").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), r#"{"items": []}"#);

        // Verify files are NOT in the wrong directories.
        assert!(storage.read("/eryx/data/helpers.py").await.is_err());
        assert!(storage.read("/eryx/lib/data.json").await.is_err());
    }

    #[tokio::test]
    async fn test_create_vfs_with_no_files() {
        let storage = create_vfs_with_files(&[], MOUNT_PATH).await;
        // Should have {mount_path}/lib and {mount_path}/data dirs but no files.
        assert!(storage.read("/eryx/lib/helpers.py").await.is_err());
        assert!(storage.read("/eryx/data/config.json").await.is_err());
    }

    #[test]
    fn test_is_safe_filename_accepts_valid_names() {
        assert!(is_safe_filename("helpers.py"));
        assert!(is_safe_filename("my_module.py"));
        assert!(is_safe_filename("data.json"));
        assert!(is_safe_filename("file-with-dashes.txt"));
        assert!(is_safe_filename("CamelCase.py"));
    }

    #[test]
    fn test_is_safe_filename_rejects_path_traversal() {
        assert!(!is_safe_filename("../../etc/passwd"));
        assert!(!is_safe_filename("../malicious.py"));
        assert!(!is_safe_filename(".."));
        assert!(!is_safe_filename("foo/../bar.py"));
    }

    #[test]
    fn test_is_safe_filename_rejects_path_separators() {
        assert!(!is_safe_filename("sub/module.py"));
        assert!(!is_safe_filename("sub\\module.py"));
        assert!(!is_safe_filename("/absolute.py"));
        assert!(!is_safe_filename("\\absolute.py"));
    }

    #[test]
    fn test_is_safe_filename_rejects_empty() {
        assert!(!is_safe_filename(""));
    }

    #[tokio::test]
    async fn test_create_vfs_skips_unsafe_filenames() {
        let files = vec![
            SupportingFile {
                name: "good.py".to_string(),
                content: "x = 1".to_string(),
                kind: FileKind::Module as i32,
            },
            SupportingFile {
                name: "../../etc/passwd".to_string(),
                content: "malicious".to_string(),
                kind: FileKind::Module as i32,
            },
            SupportingFile {
                name: "sub/nested.py".to_string(),
                content: "nested".to_string(),
                kind: FileKind::Module as i32,
            },
        ];

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await;

        // Valid file should be written.
        let content = storage.read("/eryx/lib/good.py").await.unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "x = 1");

        // Unsafe files should NOT be written anywhere.
        assert!(storage.read("/eryx/lib/../../etc/passwd").await.is_err());
        assert!(storage.read("/eryx/lib/sub/nested.py").await.is_err());
    }
}
