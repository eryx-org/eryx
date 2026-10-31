//! gRPC service implementation for the Eryx Execute RPC.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use eryx::CancellationToken;
use eryx::callback_handler::{run_callback_handler, run_net_handler};
use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
use eryx::{
    Callback, CallbackJournal, CallbackRequest, ConnectionManager, Error, NetConfig, NetRequest,
    OutputRequest, PythonStateSnapshot, ReplayState, ResourceLimits, SandboxPool, SecretConfig,
    SessionExecutor, TraceRequest, VfsConfig, scrub_placeholders, scrub_placeholders_bytes,
};
use futures::FutureExt;
use opentelemetry::propagation::Extractor;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status, Streaming};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::callbacks::{self, CallbackResult, PendingCallbacks};
use crate::proto::eryx::v1::eryx_server::EryxServer;
use crate::proto::eryx::v1::{
    CallbackOutcome, ClientMessage, ExecuteResult, ExecuteStats, FailureKind, FileKind,
    ServerMessage, SupportingFile, SuspendedCallback, callback_response, client_message,
    server_message,
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

/// Default for the largest gRPC message the server accepts or sends
/// (configurable with [`EryxService::with_max_message_size`]).
///
/// An `ExecuteRequest` can carry a state snapshot of up to 10 MiB (eryx's
/// snapshot size cap) plus code and supporting files, and the final
/// `ExecuteResult` carries a new snapshot plus the complete stdout/stderr, so
/// tonic's (and grpc-go's) 4 MiB default is too small. Clients must raise their
/// receive limit to match (e.g. grpc-go's `MaxCallRecvMsgSize`).
pub const MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Operator-set ceilings on what a request may ask for.
///
/// A request value above its ceiling is clamped down to it, not rejected, so
/// clients written against a more generous server keep working. A request that
/// leaves a limit unset gets the documented default (see the proto), which is
/// also clamped; for fuel, which is unlimited by default, the ceiling itself
/// becomes the default when one is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLimits {
    /// Longest wall-clock execution time a request may set.
    pub max_execution_timeout: Duration,
    /// Longest single-callback timeout a request may set.
    pub max_callback_timeout: Duration,
    /// Largest guest memory a request may set.
    pub max_memory_bytes: u64,
    /// Largest fuel budget a request may set, or `None` for no ceiling.
    pub max_fuel: Option<u64>,
    /// Most callback invocations a request may allow.
    pub max_callback_invocations: u32,
    /// Most concurrent network connections a request may allow.
    pub max_connections: u32,
    /// Longest network connect or I/O timeout a request may set.
    pub max_io_timeout: Duration,
    /// Most stdout plus stderr bytes kept and streamed per execution. Output
    /// past this is dropped, a truncation marker is appended, and
    /// `ExecuteResult.output_truncated` is set.
    ///
    /// Values above [`eryx::STREAM_BYTES_LIMIT`] have no effect: the sandbox
    /// stops streaming output there (counting a small overhead per write),
    /// which also sets `output_truncated`, without a marker.
    pub max_output_bytes: usize,
    /// Most host files (stdlib and site-packages modules) one execution may
    /// hold open at once. Not client-controllable: descriptors are a shared
    /// process resource.
    pub max_open_host_files: usize,
    /// Whether a request may set `network_config.allow_plaintext_secrets`,
    /// which sends real secret values over unencrypted connections. Off by
    /// default: such a request is rejected with `INVALID_ARGUMENT` rather than
    /// run with the field ignored.
    pub allow_plaintext_secrets: bool,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_execution_timeout: Duration::from_secs(300),
            max_callback_timeout: Duration::from_secs(300),
            max_memory_bytes: 512 * 1024 * 1024,
            max_fuel: None,
            max_callback_invocations: 10_000,
            max_connections: 100,
            max_io_timeout: Duration::from_secs(300),
            max_output_bytes: 16 * 1024 * 1024,
            max_open_host_files: eryx::vfs::DEFAULT_MAX_OPEN_HOST_FILES,
            allow_plaintext_secrets: false,
        }
    }
}

impl ServerLimits {
    /// Resolve a request's limits: zero fields take the defaults, then every
    /// value is clamped to its ceiling.
    fn resource_limits(
        &self,
        request: Option<&crate::proto::eryx::v1::ResourceLimits>,
    ) -> ResourceLimits {
        // `max_vfs_bytes` is deliberately absent from the proto and so never
        // client-controllable - it bounds *host* memory (VFS contents live
        // outside the guest's linear memory), so letting a request raise it
        // would hand the DoS back.
        let defaults = ResourceLimits::default();
        let request = request.copied().unwrap_or_default();
        let ms = |ms: u64| (ms > 0).then(|| Duration::from_millis(ms));
        let nonzero = |v: u64| (v > 0).then_some(v);
        defaults
            .clone()
            .with_execution_timeout(
                ms(request.execution_timeout_ms)
                    .or(defaults.execution_timeout)
                    .map(|t| t.min(self.max_execution_timeout)),
            )
            .with_callback_timeout(
                ms(request.callback_timeout_ms)
                    .or(defaults.callback_timeout)
                    .map(|t| t.min(self.max_callback_timeout)),
            )
            .with_max_memory_bytes(
                nonzero(request.max_memory_bytes)
                    .or(defaults.max_memory_bytes)
                    .map(|b| b.min(self.max_memory_bytes)),
            )
            .with_max_callback_invocations(
                (request.max_callback_invocations > 0)
                    .then_some(request.max_callback_invocations)
                    .or(defaults.max_callback_invocations)
                    .map(|n| n.min(self.max_callback_invocations)),
            )
            .with_max_fuel(match (nonzero(request.max_fuel), self.max_fuel) {
                (Some(fuel), Some(ceiling)) => Some(fuel.min(ceiling)),
                (fuel, ceiling) => fuel.or(ceiling),
            })
            .with_max_open_host_files(self.max_open_host_files)
    }
}

/// Default for how long a signed callback journal can be replayed (7 days).
pub const DEFAULT_JOURNAL_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The Eryx gRPC service.
///
/// Holds a shared [`SandboxPool`] for efficient sandbox reuse across requests.
#[derive(Debug)]
pub struct EryxService {
    pool: Arc<SandboxPool>,
    journal_signer: crate::replay::JournalSigner,
    max_message_size: usize,
    limits: ServerLimits,
    journal_ttl: Duration,
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
            max_message_size: MAX_MESSAGE_SIZE,
            limits: ServerLimits::default(),
            journal_ttl: DEFAULT_JOURNAL_TTL,
        }
    }

    /// Set the ceilings applied to every request's limits. Defaults to
    /// [`ServerLimits::default`].
    #[must_use]
    pub fn with_limits(mut self, limits: ServerLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Set how long a signed callback journal stays valid for replay.
    /// Defaults to [`DEFAULT_JOURNAL_TTL`].
    #[must_use]
    pub fn with_journal_ttl(mut self, ttl: Duration) -> Self {
        self.journal_ttl = ttl;
        self
    }

    /// Set the largest gRPC message the server accepts or sends, in bytes.
    /// Defaults to [`MAX_MESSAGE_SIZE`].
    #[must_use]
    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = bytes;
        self
    }

    /// Create a new service with a random ephemeral signing key.
    ///
    /// Journals signed by this instance cannot be verified by other replicas or
    /// after a restart. Intended for tests and single-instance dev servers.
    #[must_use]
    pub fn new(pool: Arc<SandboxPool>) -> Self {
        Self::with_signer(pool, crate::replay::JournalSigner::random())
    }

    /// Wrap the service in a tonic server with the maximum message size applied
    /// to both decoding and encoding.
    #[must_use]
    pub fn into_server(self) -> EryxServer<Self> {
        let max = self.max_message_size;
        EryxServer::new(self)
            .max_decoding_message_size(max)
            .max_encoding_message_size(max)
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
            .await?
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

        let binding = crate::replay::JournalBinding {
            code: &code,
            state: persist_state.then_some(state_snapshot.as_slice()),
            callbacks: &declarations,
        };
        // A rejected replay must not become a fresh run: completed callbacks
        // and direct script effects may already have happened.
        let previous_journal: Option<CallbackJournal> = execute_req
            .callback_journal
            .as_ref()
            .map(|journal| {
                if (!journal.entries.is_empty() || !journal.signature.is_empty())
                    && !self.journal_signer.verify(journal, binding)
                {
                    return Err(Status::failed_precondition(
                        "callback journal signature verification failed (expired, or signed \
                         for a different script, state or callback set); replay was not executed",
                    ));
                }
                Ok(crate::replay::journal_from_proto(&code, journal))
            })
            .transpose()?;

        // Parse network config: present = networking enabled, absent = disabled.
        let net_config = execute_req
            .network_config
            .map(|nc| -> Result<NetConfig, Status> {
                if nc.allow_plaintext_secrets && !self.limits.allow_plaintext_secrets {
                    return Err(Status::invalid_argument(
                        "network_config.allow_plaintext_secrets is not enabled on this server \
                         (start it with --allow-plaintext-secrets)",
                    ));
                }
                let base = NetConfig::default();
                let max_connections = if nc.max_connections > 0 {
                    nc.max_connections
                } else {
                    base.max_connections()
                };
                let connect_timeout = if nc.connect_timeout_ms > 0 {
                    Duration::from_millis(nc.connect_timeout_ms)
                } else {
                    base.connect_timeout()
                };
                let io_timeout = if nc.io_timeout_ms > 0 {
                    Duration::from_millis(nc.io_timeout_ms)
                } else {
                    base.io_timeout()
                };
                let mut config = base
                    .allow_all_hosts(nc.allow_all_hosts)
                    .with_allowed_hosts(nc.allowed_hosts)
                    .with_max_connections(max_connections.min(self.limits.max_connections))
                    .with_connect_timeout(connect_timeout.min(self.limits.max_io_timeout))
                    .with_io_timeout(io_timeout.min(self.limits.max_io_timeout))
                    .with_allowed_networks(parse_networks(
                        "allowed_networks",
                        &nc.allowed_networks,
                    )?)
                    .with_internal_hosts(nc.internal_hosts)
                    .allow_plaintext_secrets(nc.allow_plaintext_secrets);
                if !nc.blocked_hosts.is_empty() {
                    config = config.with_blocked_hosts(nc.blocked_hosts);
                }
                if !nc.blocked_networks.is_empty() {
                    config = config.with_blocked_networks(parse_networks(
                        "blocked_networks",
                        &nc.blocked_networks,
                    )?);
                }
                Ok(config)
            })
            .transpose()?;

        // Parse and validate secrets: generate placeholders and build env preamble.
        let (secrets, secrets_preamble) = {
            let mut map = HashMap::new();
            let mut preamble = String::new();
            if !execute_req.secrets.is_empty() {
                preamble.push_str("import os\n");
            }
            for s in &execute_req.secrets {
                // Validate secret name: must be a valid Python env var name.
                if !is_identifier(&s.name) {
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
                let secret = SecretConfig::new(s.value.clone(), s.allowed_hosts.clone());
                preamble.push_str(&format!(
                    "os.environ[{:?}] = {:?}\n",
                    s.name,
                    secret.placeholder()
                ));
                map.insert(s.name.clone(), secret);
            }
            (map, preamble)
        };
        // A secret with no host to go to would never be substituted; say so
        // now rather than letting the script fail later.
        if let Some(config) = &net_config {
            config.validate().map_err(Status::invalid_argument)?;
            config
                .check_secret_hosts(&secrets)
                .map_err(Status::invalid_argument)?;
        }
        callbacks::validate_declarations(&declarations).map_err(Status::invalid_argument)?;
        if !result_variable.is_empty() && !is_identifier(&result_variable) {
            return Err(Status::invalid_argument(format!(
                "invalid result_variable {result_variable:?}: must match [A-Za-z_][A-Za-z0-9_]*"
            )));
        }
        let vfs_storage = if files.is_empty() {
            None
        } else {
            Some(create_vfs_with_files(&files, VFS_MOUNT_PATH).await?)
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

        // 2. Parse resource limits, clamped to the server's ceilings.
        let resource_limits = self
            .limits
            .resource_limits(execute_req.resource_limits.as_ref());

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
        let inbound_closed = CancellationToken::new();
        let cbs = callbacks::build_callbacks(
            &declarations,
            server_tx.clone(),
            Arc::clone(&pending),
            inbound_closed.clone(),
        );

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
            .map_err(|e| Status::invalid_argument(e.to_string()))?
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
        // execution task when the inbound stream fails (client disconnected).
        let cancel = CancellationToken::new();

        // 6. Spawn callback dispatch task: reads inbound CallbackResponses and
        //    routes them to the pending oneshot senders.
        let pending_dispatch = Arc::clone(&pending);
        let cancel_dispatch = cancel.clone();
        tokio::spawn(
            async move {
                let disconnected = loop {
                    let msg = match inbound.message().await {
                        Ok(Some(msg)) => msg,
                        // Half-close: the client has nothing more to send
                        // (e.g. Go's `CloseSend()`), but is still reading the
                        // response stream, so let the execution finish.
                        Ok(None) => break false,
                        Err(_) => break true,
                    };
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
                };
                // No further callback responses can arrive — fail pending and
                // future callbacks immediately instead of letting them wait for
                // callback_timeout.
                inbound_closed.cancel();
                let remaining = pending_dispatch.len();
                if remaining > 0 {
                    tracing::warn!(
                        pending_callbacks = remaining,
                        disconnected,
                        "client stopped sending with pending callbacks, cancelling them"
                    );
                    pending_dispatch.clear();
                } else {
                    tracing::info!(disconnected, "client stopped sending");
                }
                if disconnected {
                    // Signal the execution task that the client is gone.
                    cancel_dispatch.cancel();
                }
            }
            .instrument(span.clone()),
        );

        // 7. Spawn execution task.
        let journal_signer = self.journal_signer.clone();
        let max_output_bytes = self.limits.max_output_bytes;
        let journal_ttl = self.journal_ttl;
        let server_tx_result = server_tx;
        let resp_tx_final = resp_tx;
        let cancel_exec = cancel;
        let sandbox_held_start = Instant::now();
        tokio::spawn(
            async move {
                let params = SessionParams {
                    code: &code,
                    vfs_storage,
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
                    journal_ttl,
                    declarations: &declarations,
                    max_output_bytes,
                };

                // Race execution against client disconnection: either the
                // inbound stream failing or the response stream being dropped.
                // If the client is gone, abandon execution and release the
                // sandbox promptly instead of waiting for timeouts to expire.
                let execution = AssertUnwindSafe(execute_with_session(
                    &mut sandbox,
                    &params,
                    &server_tx_result,
                    &cancel_exec,
                ))
                .catch_unwind();
                let result_msg = tokio::select! {
                    result = execution => result,
                    () = cancel_exec.cancelled() => Ok(cancelled_result()),
                    () = resp_tx_final.closed() => Ok(cancelled_result()),
                };
                let panicked = result_msg.is_err();

                match result_msg {
                    Ok(result_msg) => {
                        let msg = ServerMessage {
                            message: Some(server_message::Message::ExecuteResult(Box::new(
                                result_msg,
                            ))),
                        };
                        // Send via internal channel first, then drop it so the forwarder ends.
                        let _ = server_tx_result.send(msg).await;
                    }
                    Err(_) => {
                        tracing::error!("execution panicked");
                        metrics::counter!("eryx_executions_total", "status" => "panic")
                            .increment(1);
                        // Without this the stream would end with OK status and no
                        // ExecuteResult.
                        let _ = resp_tx_final
                            .send(Err(Status::internal("execution failed unexpectedly")))
                            .await;
                    }
                }
                drop(server_tx_result);

                let sandbox_held_ms = sandbox_held_start.elapsed().as_millis() as u64;
                if panicked {
                    // The panic may have left the sandbox half-configured, so
                    // keep it out of the pool; the pool builds a fresh one.
                    tracing::debug!(sandbox_held_ms, "discarding sandbox after panic");
                    drop(sandbox.detach());
                } else {
                    // Per-request state is cleared and the sandbox returns to the pool.
                    tracing::debug!(
                        sandbox_held_ms,
                        "execution task finished, returning sandbox to pool"
                    );
                    drop(sandbox);
                }

                // The resp_tx_final is kept alive to ensure the response stream stays
                // open until this task completes. Dropping it signals stream end.
                drop(resp_tx_final);
            }
            .instrument(span.clone()),
        );

        Ok(Response::new(ReceiverStream::new(resp_rx)))
    }
}

/// The result reported when the client disconnects before execution finishes.
fn cancelled_result() -> ExecuteResult {
    tracing::warn!("client disconnected, aborting execution");
    metrics::counter!("eryx_executions_cancelled_total").increment(1);
    metrics::counter!("eryx_executions_total", "status" => "cancelled").increment(1);
    ExecuteResult {
        success: false,
        error: "execution cancelled: client disconnected".to_string(),
        failure_kind: FailureKind::Cancelled as i32,
        ..Default::default()
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

/// Parse a `NetworkConfig` CIDR list, naming the field in the error.
fn parse_networks(field: &str, cidrs: &[String]) -> Result<Vec<eryx::IpNet>, Status> {
    cidrs
        .iter()
        .map(|c| {
            c.parse()
                .map_err(|e| Status::invalid_argument(format!("{field}: invalid CIDR '{c}': {e}")))
        })
        .collect()
}

/// Check whether a name is an ASCII Python identifier (`[A-Za-z_][A-Za-z0-9_]*`).
///
/// Used for secret (environment variable) names, callback names and the
/// result variable.
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check whether a filename is safe for use in VFS paths.
///
/// Rejects names that contain path separators (`/`, `\\`), `..` components or
/// NUL bytes, which could be used to escape the intended directory, and `.`,
/// which names the directory itself.
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
}

/// Mount point of the VFS holding supporting files. Must match the VFS
/// preopen path, since guest paths resolve beneath the preopen they use.
const VFS_MOUNT_PATH: &str = "/eryx";

/// Create a VFS storage pre-populated with the given supporting files.
///
/// Files are routed into subdirectories under the VFS mount based on their kind:
/// - `MODULE` files go to `{mount_path}/lib/<name>` (added to `sys.path`)
/// - `DATA` files go to `{mount_path}/data/<name>` (readable but not importable)
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if a file has an unsafe name, two files of the
/// same kind share a name, or a file cannot be written to the VFS.
async fn create_vfs_with_files(
    files: &[SupportingFile],
    mount_path: &str,
) -> Result<ArcStorage, Status> {
    let storage = ArcStorage::new(Arc::new(InMemoryStorage::new()) as Arc<dyn VfsStorage>);

    let lib_path = format!("{mount_path}/lib");
    let data_path = format!("{mount_path}/data");

    // Create subdirectories under the mount path. The VFS preopen will also
    // create the mount_path dir itself via add_vfs_preopen, but we need the
    // sub-dirs to exist before writing files.
    let _ = storage.mkdir_sync(&lib_path);
    let _ = storage.mkdir_sync(&data_path);

    let mut paths = std::collections::HashSet::new();
    let mut total_bytes: usize = 0;

    for f in files {
        if !is_safe_filename(&f.name) {
            return Err(Status::invalid_argument(format!(
                "invalid supporting file name {:?}: must be a plain file name \
                 (no path separators, '..', '.' or NUL bytes)",
                f.name
            )));
        }
        let dir = match f.kind() {
            FileKind::Module => &lib_path,
            FileKind::Data => &data_path,
        };
        let path = format!("{dir}/{}", f.name);
        if !paths.insert(path.clone()) {
            return Err(Status::invalid_argument(format!(
                "duplicate supporting file {:?} of kind {:?}",
                f.name,
                f.kind()
            )));
        }
        storage
            .write(&path, f.content.as_bytes())
            .await
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "failed to write supporting file {:?}: {e}",
                    f.name
                ))
            })?;
        tracing::debug!(file = %f.name, kind = ?f.kind(), dir, size = f.content.len(), "wrote supporting file to VFS");
        total_bytes += f.content.len();
    }

    tracing::info!(
        mounted_files = files.len(),
        total_bytes,
        "VFS file mounting complete"
    );

    Ok(storage)
}

/// Parameters for a single session execution.
struct SessionParams<'a> {
    code: &'a str,
    /// VFS pre-populated with the request's supporting files, if any.
    vfs_storage: Option<ArcStorage>,
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
    /// How long a signed outgoing journal stays valid.
    journal_ttl: Duration,
    /// The request's callback declarations, which outgoing journals are bound to.
    declarations: &'a [crate::proto::eryx::v1::CallbackDeclaration],
    /// Cap on stdout plus stderr bytes kept and streamed.
    max_output_bytes: usize,
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
    cancel: &CancellationToken,
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
        .map(|journal| ReplayState::shared(params.code, Some(journal.clone())));
    let active_callbacks: HashMap<String, Arc<dyn Callback>> = match &replay_state {
        Some(state) => eryx::replay::wrap_callbacks(callbacks_ref, state),
        None => callbacks_ref.clone(),
    };
    let callbacks_arc: Vec<Arc<dyn Callback>> = active_callbacks.values().cloned().collect();

    // Create the session executor, optionally with pre-populated VFS for supporting files.
    let mut session = match params.vfs_storage.clone() {
        None => match SessionExecutor::new_with_limits(
            executor,
            &callbacks_arc,
            &params.resource_limits,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    code_len = params.code.len(),
                    "failed to create session executor"
                );
                return ExecuteResult {
                    success: false,
                    error: format!("session creation failed: {e}"),
                    failure_kind: FailureKind::SandboxError as i32,
                    ..Default::default()
                };
            }
        },
        Some(vfs_storage) => {
            let vfs_config = VfsConfig::new(VFS_MOUNT_PATH);
            match SessionExecutor::new_with_vfs_config_and_limits(
                executor,
                &callbacks_arc,
                vfs_storage,
                vfs_config,
                &params.resource_limits,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        code_len = params.code.len(),
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
        }
    };

    // A disconnect also stops guest code run by the restore and snapshot.
    session.set_state_op_cancellation(Some(cancel.clone()));

    // Restore previous state if provided. A snapshot that cannot be restored
    // (corrupt, or from an incompatible runtime) does not fail the request -
    // the code runs against a clean state - but is reported in `state_error`
    // so the caller can tell its state was lost.
    let mut state_errors = Vec::new();
    if let Some(state_snapshot) = params.state.filter(|s| !s.is_empty()) {
        let restore_start = Instant::now();
        let restored = match PythonStateSnapshot::from_bytes(state_snapshot) {
            Ok(snapshot) => session.restore_state(&snapshot).await,
            Err(e) => Err(e),
        };
        match restored {
            Ok(()) => tracing::info!(
                restore_ms = restore_start.elapsed().as_millis() as u64,
                snapshot_bytes = state_snapshot.len(),
                "state restored from snapshot"
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    restore_ms = restore_start.elapsed().as_millis() as u64,
                    snapshot_bytes = state_snapshot.len(),
                    "failed to restore state, proceeding with clean state"
                );
                state_errors.push(format!(
                    "failed to restore state snapshot, executed with a clean state: {e}"
                ));
            }
        }
    }

    // Apply a per-request result-variable override. Done *after* restore_state:
    // although restore_state reuses the current instance today, applying the
    // override last keeps it correct even if restore were to reset the instance.
    if !params.result_variable.is_empty()
        && let Err(e) = session.set_result_variable(&params.result_variable).await
    {
        tracing::warn!(error = %e, "failed to set result variable");
        return ExecuteResult {
            success: false,
            error: format!("failed to set result variable: {e}"),
            failure_kind: FailureKind::SandboxError as i32,
            ..Default::default()
        };
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

    // Set up trace channel if tracing is enabled.
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel::<TraceRequest>();
    if params.enable_tracing {
        let trace_server_tx = server_tx.clone();
        let trace_secrets = params.secrets.clone();
        tokio::spawn(async move {
            while let Some(req) = trace_rx.recv().await {
                let msg = ServerMessage {
                    message: Some(server_message::Message::TraceEvent(trace_event(
                        &req,
                        &trace_secrets,
                    ))),
                };
                if trace_server_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
    }

    // Stream output to the client as it arrives, and keep the raw (pre-scrub)
    // bytes for the final ExecuteResult. Both stop at `max_output_bytes`.
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<OutputRequest>();
    let output_server_tx = server_tx.clone();
    let output_scrub_stdout = params.scrub_stdout;
    let output_scrub_stderr = params.scrub_stderr;
    let output_secrets = if output_scrub_stdout || output_scrub_stderr {
        Some(params.secrets.clone())
    } else {
        None
    };
    let max_output_bytes = params.max_output_bytes;
    let output_accumulator = tokio::spawn(async move {
        use crate::proto::eryx::v1::{OutputEvent, OutputStream};
        let mut output = CappedOutput::new(max_output_bytes);
        // Once the client stops reading, keep draining so the cap still applies.
        let mut forwarding = true;
        while let Some(req) = output_rx.recv().await {
            let stream = if req.stream == 0 {
                OutputStream::Stdout
            } else {
                OutputStream::Stderr
            };
            let Some(chunk) = output.push(stream, &req.data) else {
                continue;
            };
            if !forwarding {
                continue;
            }
            let should_scrub = match stream {
                OutputStream::Stdout => output_scrub_stdout,
                OutputStream::Stderr => output_scrub_stderr,
            };
            let data = match &output_secrets {
                Some(secrets) if should_scrub => scrub_placeholders_bytes(&chunk, secrets),
                _ => chunk,
            };
            let msg = ServerMessage {
                message: Some(server_message::Message::OutputEvent(OutputEvent {
                    stream: stream.into(),
                    data,
                })),
            };
            forwarding = output_server_tx.send(msg).await.is_ok();
        }
        output
    });

    // Spawn network handler if networking is enabled (mirrors Sandbox::execute pattern).
    let (net_tx, _net_handler) = if let Some(config) = params.net_config.clone() {
        let (tx, rx) = mpsc::channel::<NetRequest>(32);
        let allowed_hosts = config.allowed_hosts().len();
        let max_connections = config.max_connections();
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

    // Run builtins injection before the user code so callbacks are accessible
    // from imported modules. Also add sys.path setup when supporting files are
    // present — merged here to avoid a separate WASM execution cycle. As a
    // preamble, it doesn't shift the user code's line numbers.
    let preamble = if params.vfs_storage.is_some() {
        format!(
            "{}{SYS_PATH_INJECT}{BUILTINS_INJECT}",
            params.secrets_preamble
        )
    } else {
        format!("{}{BUILTINS_INJECT}", params.secrets_preamble)
    };
    let mut builder = session
        .execute(params.code)
        .with_preamble(preamble)
        .with_callbacks(&callbacks_arc, callback_tx)
        .with_output_streaming(output_tx)
        // Lets a disconnect interrupt CPU-bound code too; without a token the
        // guest only stops at the execution timeout.
        .with_cancellation(cancel.clone());
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
    // stdout/stderr populate the result on both paths; the copy the guest
    // returns on success is dropped, so the output is held only once.
    let output = output_accumulator
        .await
        .unwrap_or_else(|_| CappedOutput::new(0));
    // The sandbox's own stream budget can also drop output.
    let output_truncated = output.truncated || session.last_output_truncated();
    let stdout = if params.scrub_stdout {
        scrub_placeholders_bytes(&output.stdout, &params.secrets)
    } else {
        output.stdout
    };
    let stderr = if params.scrub_stderr {
        scrub_placeholders_bytes(&output.stderr, &params.secrets)
    } else {
        output.stderr
    };

    // Extract the recorded replay journal. Read regardless of success/failure so
    // the caller can replay completed callbacks even when a later one errored or
    // suspended. The suspension metadata (if any) is read from the same guard.
    let (proto_journal, replayed_callbacks, suspended_callback) = match &replay_state {
        Some(state) => {
            let guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut journal = crate::replay::journal_to_proto(&guard.build_journal(params.code));
            let binding = crate::replay::JournalBinding {
                code: params.code,
                state: params.state,
                callbacks: params.declarations,
            };
            params
                .journal_signer
                .sign(&mut journal, binding, params.journal_ttl);
            let suspended = guard.suspended().map(crate::replay::suspended_to_proto);
            (Some(journal), guard.replayed_count(), suspended)
        }
        None => (None, 0, None),
    };
    // A client can reply CALLBACK_OUTCOME_SUSPEND even without opting into replay.
    // In that case there is no ReplayState to record the suspended callback's
    // name/args, but the executor still returns Error::Suspended. Derive the
    // suspension from the error so the outcome is reported as a suspension (with
    // its reason) rather than mis-surfaced as an ordinary sandbox error. There is
    // no prefix journal to resume from in this mode, but the signal is honest.
    let suspended_callback = suspended_callback.or_else(|| match &exec_result {
        Err(Error::Suspended(reason)) => Some(SuspendedCallback {
            name: String::new(),
            args_json: String::new(),
            reason: reason.clone(),
        }),
        _ => None,
    });
    let suspended = suspended_callback.is_some();

    // Snapshot state after execution (only when persistence is requested, and not
    // after a suspension: the guest's fuel is poisoned to 0 once a callback
    // suspends, so invoking the snapshot_state export would trap immediately
    // (OutOfFuel) and yield a misleading empty snapshot — which could clobber
    // prior session state. Resume goes through journal replay, not the snapshot,
    // so the snapshot is not needed here anyway.)
    let (snapshot_bytes, state_skipped_globals) = if params.state.is_some() && !suspended {
        let snapshot_start = Instant::now();
        match session.snapshot_state().await {
            Ok(snapshot) => {
                let bytes = snapshot.to_bytes();
                tracing::info!(
                    snapshot_bytes = bytes.len(),
                    snapshot_ms = snapshot_start.elapsed().as_millis() as u64,
                    skipped_globals = ?snapshot.skipped_globals(),
                    "state snapshot captured"
                );
                (bytes, snapshot.skipped_globals().to_vec())
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    snapshot_ms = snapshot_start.elapsed().as_millis() as u64,
                    "failed to capture state snapshot"
                );
                state_errors.push(format!("failed to capture state snapshot: {e}"));
                (Vec::new(), Vec::new())
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let duration = start.elapsed();
    let state_error = state_errors.join("; ");

    match exec_result {
        Ok(exec_output) => {
            tracing::info!(
                success = true,
                duration_ms = duration.as_millis() as u64,
                callback_invocations,
                snapshot_bytes = snapshot_bytes.len(),
                "session execution completed"
            );
            metrics::counter!("eryx_executions_total", "status" => "success").increment(1);
            metrics::histogram!("eryx_execution_duration_seconds").record(duration.as_secs_f64());
            // The structured result is a programmatic side channel, so scrubbing is
            // opt-in (params.scrub_result), independent of the stdout/stderr policy.
            // When enabled, scrub both the result and its error message.
            let (result, result_error) = if params.scrub_result {
                (
                    exec_output
                        .result
                        .map(|r| scrub_placeholders(&r, &params.secrets))
                        .unwrap_or_default(),
                    exec_output
                        .result_error
                        .map(|e| scrub_placeholders(&e, &params.secrets))
                        .unwrap_or_default(),
                )
            } else {
                (
                    exec_output.result.unwrap_or_default(),
                    exec_output.result_error.unwrap_or_default(),
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
                    peak_memory_bytes: exec_output.peak_memory_bytes,
                    fuel_consumed: exec_output.fuel_consumed.unwrap_or(0),
                    replayed_callbacks,
                }),
                state_snapshot: snapshot_bytes,
                result,
                result_error,
                failure_kind: FailureKind::Unspecified as i32,
                callback_journal: proto_journal,
                suspended,
                suspended_callback,
                state_error,
                output_truncated,
                state_skipped_globals,
                state_was_reset: exec_output.state_was_reset,
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
                stats: Some(ExecuteStats {
                    duration_ms: duration.as_millis() as u64,
                    callback_invocations,
                    // There is no `output` on the error path, so read the
                    // stats the session kept for its last execution. A
                    // suspension poisons the remaining fuel, so the consumed
                    // figure would be meaningless there.
                    peak_memory_bytes: session.last_peak_memory_bytes(),
                    fuel_consumed: if suspended {
                        0
                    } else {
                        session.last_fuel_consumed().unwrap_or(0)
                    },
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
                state_error,
                output_truncated,
                state_skipped_globals,
                state_was_reset: false,
            }
        }
    }
}

/// Appended to the stream whose output crossed `max_output_bytes`.
fn truncation_marker(max: usize) -> Vec<u8> {
    format!("\n[output truncated: exceeded {max} bytes]\n").into_bytes()
}

/// Stdout and stderr captured for one execution, capped at a combined size.
#[derive(Debug)]
struct CappedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    remaining: usize,
    max: usize,
    truncated: bool,
}

impl CappedOutput {
    fn new(max: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            remaining: max,
            max,
            truncated: false,
        }
    }

    /// Record a chunk and return the bytes to stream for it: the part that
    /// fits, plus the truncation marker on the chunk that crosses the cap.
    /// Returns `None` once output has been truncated.
    fn push(
        &mut self,
        stream: crate::proto::eryx::v1::OutputStream,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        if self.truncated {
            return None;
        }
        let mut chunk = data[..data.len().min(self.remaining)].to_vec();
        self.remaining -= chunk.len();
        if chunk.len() < data.len() {
            self.truncated = true;
            chunk.extend(truncation_marker(self.max));
        }
        match stream {
            crate::proto::eryx::v1::OutputStream::Stdout => &mut self.stdout,
            crate::proto::eryx::v1::OutputStream::Stderr => &mut self.stderr,
        }
        .extend_from_slice(&chunk);
        Some(chunk)
    }
}

/// Classify a library [`Error`] into the proto [`FailureKind`].
///
/// [`Error::PythonException`] is a *script* failure — its traceback is surfaced
/// in `stderr` — so it maps to [`FailureKind::ScriptException`].
/// [`Error::Suspended`] is an expected control outcome (a callback deferred), not
/// a failure, so it maps to [`FailureKind::Suspended`]; callers should branch on
/// the `suspended` flag first. Everything else is a sandbox-level failure whose
/// message belongs in the `error` field.
fn classify_failure(error: &Error) -> FailureKind {
    match error {
        Error::PythonException(_) => FailureKind::ScriptException,
        Error::Timeout(_) => FailureKind::Timeout,
        Error::FuelExhausted { .. } => FailureKind::FuelExhausted,
        Error::Cancelled => FailureKind::Cancelled,
        Error::Suspended(_) => FailureKind::Suspended,
        _ => FailureKind::SandboxError,
    }
}

/// Convert a guest trace event to its proto form, scrubbing secret
/// placeholders from its text fields.
fn trace_event(
    req: &TraceRequest,
    secrets: &HashMap<String, SecretConfig>,
) -> crate::proto::eryx::v1::TraceEvent {
    let (event_type, function, message, name, duration_ms) =
        parse_trace_event_json(&req.event_json);
    let scrub = |s: String| {
        if secrets.is_empty() {
            s
        } else {
            scrub_placeholders(&s, secrets)
        }
    };
    crate::proto::eryx::v1::TraceEvent {
        lineno: req.lineno,
        event_type: scrub(event_type),
        function: scrub(function),
        message: scrub(message),
        name: scrub(name),
        duration_ms,
        context_json: String::new(),
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

    #[test]
    fn trace_events_are_scrubbed() {
        let secret = SecretConfig::new("real", vec![]);
        let placeholder = secret.placeholder().to_string();
        let secrets = HashMap::from([("TOKEN".to_string(), secret)]);
        let req = TraceRequest {
            lineno: 1,
            event_json:
                serde_json::json!({"type": "exception", "message": format!("bad {placeholder}")})
                    .to_string(),
            context_json: String::new(),
        };
        let event = trace_event(&req, &secrets);
        assert_eq!(event.event_type, "exception");
        assert!(!event.message.contains(&placeholder), "{}", event.message);
        assert!(event.message.starts_with("bad "), "{}", event.message);
    }

    #[test]
    fn server_limits_fill_defaults_and_clamp() {
        use crate::proto::eryx::v1::ResourceLimits as ProtoLimits;

        let ceilings = ServerLimits {
            max_execution_timeout: Duration::from_secs(10),
            max_memory_bytes: 64,
            max_fuel: Some(1000),
            ..Default::default()
        };
        let unset = ceilings.resource_limits(None);
        assert_eq!(unset.execution_timeout, Some(Duration::from_secs(10)));
        assert_eq!(unset.max_memory_bytes, Some(64));
        assert_eq!(
            unset.max_fuel,
            Some(1000),
            "the fuel ceiling is the default"
        );
        assert_eq!(
            unset.callback_timeout,
            ResourceLimits::default().callback_timeout
        );

        let request = ProtoLimits {
            execution_timeout_ms: 5_000,
            max_memory_bytes: 1 << 40,
            max_fuel: 10,
            ..Default::default()
        };
        let limits = ceilings.resource_limits(Some(&request));
        assert_eq!(
            limits.max_open_host_files,
            Some(eryx::vfs::DEFAULT_MAX_OPEN_HOST_FILES)
        );
        assert_eq!(limits.execution_timeout, Some(Duration::from_secs(5)));
        assert_eq!(limits.max_memory_bytes, Some(64));
        assert_eq!(limits.max_fuel, Some(10));

        let no_fuel_ceiling = ServerLimits::default().resource_limits(None);
        assert_eq!(no_fuel_ceiling.max_fuel, None);
        assert_eq!(
            no_fuel_ceiling.max_memory_bytes,
            ResourceLimits::default().max_memory_bytes
        );
    }

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

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await.unwrap();

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

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await.unwrap();

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

        let storage = create_vfs_with_files(&files, MOUNT_PATH).await.unwrap();

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
        let storage = create_vfs_with_files(&[], MOUNT_PATH).await.unwrap();
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

    #[test]
    fn test_is_safe_filename_rejects_dot_and_nul() {
        assert!(!is_safe_filename("."));
        assert!(!is_safe_filename("a\0b.py"));
    }

    #[tokio::test]
    async fn test_create_vfs_rejects_unsafe_filenames() {
        for name in ["../../etc/passwd", "sub/nested.py", ".", ""] {
            let files = vec![
                SupportingFile {
                    name: "good.py".to_string(),
                    content: "x = 1".to_string(),
                    kind: FileKind::Module as i32,
                },
                SupportingFile {
                    name: name.to_string(),
                    content: "bad".to_string(),
                    kind: FileKind::Module as i32,
                },
            ];
            let err = create_vfs_with_files(&files, MOUNT_PATH).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{name:?}");
        }
    }

    #[tokio::test]
    async fn test_create_vfs_rejects_duplicates_per_kind() {
        let file = |kind: FileKind| SupportingFile {
            name: "same.py".to_string(),
            content: "x = 1".to_string(),
            kind: kind as i32,
        };
        // The same name is fine across kinds (different directories)...
        create_vfs_with_files(&[file(FileKind::Module), file(FileKind::Data)], MOUNT_PATH)
            .await
            .unwrap();
        // ...but not twice within one.
        let err = create_vfs_with_files(&[file(FileKind::Data), file(FileKind::Data)], MOUNT_PATH)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("duplicate"), "{}", err.message());
    }
}
