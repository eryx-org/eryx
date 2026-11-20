//! gRPC service implementation for the Eryx Execute RPC.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use eryx::CancellationToken;
use eryx::callback_handler::{run_callback_handler, run_net_handler};
use eryx::vfs::{ArcStorage, InMemoryStorage, VfsStorage};
use eryx::{
    Callback, CallbackJournal, CallbackRequest, ConnectionManager, Error, ErrorKind, HttpRule,
    NetConfig, NetRequest, OutputRequest, ReplayState, ResourceLimits, SandboxPool, SecretConfig,
    SessionExecutor, StateSnapshot, TraceRequest, VfsConfig, scrub_placeholders,
    scrub_placeholders_bytes,
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
    /// Most concurrent network connections a request may allow, its guest's
    /// and its children's together.
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
    /// Most host memory a shell execution's child processes' arguments and
    /// environments may hold at once, and the value for requests that set
    /// none (`resource_limits.max_child_host_bytes`).
    pub max_child_host_bytes: u64,
    /// Most memory a shell execution's running child processes may hold
    /// together, and the value for requests that set none
    /// (`resource_limits.max_child_memory_bytes`).
    pub max_child_memory_bytes: u64,
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
            max_child_host_bytes: 64 * 1024 * 1024,
            max_child_memory_bytes: 256 * 1024 * 1024,
            allow_plaintext_secrets: false,
        }
    }
}

impl ServerLimits {
    /// Parse a request's `network_config`, clamped to these limits, and the
    /// network it gives the shell's children.
    fn network_with_children(
        &self,
        mut nc: crate::proto::eryx::v1::NetworkConfig,
    ) -> Result<(NetConfig, eryx::net::ChildNetwork), Status> {
        let child = *nc.child_network.take().unwrap_or_default();
        let config = self.network(nc)?;
        let child = self.child_network("network_config.child_network", child)?;
        Ok((config, child))
    }

    /// Parse a `ChildNetwork` (at `field`), its narrow config clamped to
    /// these limits.
    fn child_network(
        &self,
        field: &str,
        child: crate::proto::eryx::v1::ChildNetwork,
    ) -> Result<eryx::net::ChildNetwork, Status> {
        use crate::proto::eryx::v1::child_network::Mode;
        let invalid = |msg: &str| Err(Status::invalid_argument(format!("{field}: {msg}")));
        Ok(match (child.mode(), child.narrow) {
            (Mode::None, None) => eryx::net::ChildNetwork::None,
            (Mode::Inherit, None) => eryx::net::ChildNetwork::Inherit,
            (Mode::Narrow, Some(narrow)) => {
                if narrow.child_network.is_some() || narrow.allow_plaintext_secrets {
                    return invalid("narrow can't set child_network or allow_plaintext_secrets");
                }
                eryx::net::ChildNetwork::Narrow(Box::new(self.network(*narrow)?))
            }
            (Mode::Narrow, None) => return invalid("MODE_NARROW needs narrow"),
            (_, Some(_)) => return invalid("narrow is only for MODE_NARROW"),
        })
    }

    /// Parse a shell request's `commands`: each must be one of `known` (when
    /// the server knows its commands), and may only take away from what it
    /// has, so no network when `children` (the request's child network) is
    /// none.
    fn command_options(
        &self,
        commands: HashMap<String, crate::proto::eryx::v1::CommandOptions>,
        known: Option<&HashSet<String>>,
        children: &eryx::net::ChildNetwork,
    ) -> Result<HashMap<String, eryx::CommandOptions>, Status> {
        let mut parsed = HashMap::new();
        for (name, options) in commands {
            if known.is_some_and(|known| !known.contains(&name)) {
                return Err(Status::invalid_argument(format!(
                    "commands: {name} isn't one of this server's shell commands"
                )));
            }
            let mut command = eryx::CommandOptions::new();
            if let Some(network) = options.network {
                let network = self.child_network(&format!("commands[{name}].network"), network)?;
                if matches!(children, eryx::net::ChildNetwork::None)
                    && !matches!(network, eryx::net::ChildNetwork::None)
                {
                    return Err(Status::invalid_argument(format!(
                        "commands[{name}].network: can't give a command network when \
                         network_config.child_network gives children none"
                    )));
                }
                command = command.with_network(network);
            }
            if options.no_tunnels {
                command = command.without_tunnels();
            }
            if options.no_spawn {
                command = command.without_spawn();
            }
            parsed.insert(name, command);
        }
        Ok(parsed)
    }

    /// Parse one `NetworkConfig`, clamped to these limits.
    fn network(&self, nc: crate::proto::eryx::v1::NetworkConfig) -> Result<NetConfig, Status> {
        if nc.allow_plaintext_secrets && !self.allow_plaintext_secrets {
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
            .with_max_connections(max_connections.min(self.max_connections))
            .with_connect_timeout(connect_timeout.min(self.max_io_timeout))
            .with_io_timeout(io_timeout.min(self.max_io_timeout))
            .with_allowed_networks(parse_networks("allowed_networks", &nc.allowed_networks)?)
            .with_internal_hosts(nc.internal_hosts)
            .with_http_rules(nc.http_rules.into_iter().map(|r| {
                HttpRule::new(r.host)
                    .with_methods(r.methods)
                    .with_paths(r.paths)
            }))
            .allow_plaintext_secrets(nc.allow_plaintext_secrets);
        if !nc.blocked_hosts.is_empty() {
            config = config.with_blocked_hosts(nc.blocked_hosts);
        }
        if !nc.blocked_networks.is_empty() {
            config = config
                .with_blocked_networks(parse_networks("blocked_networks", &nc.blocked_networks)?);
        }
        config
            .validate()
            .map_err(|e| Status::invalid_argument(format!("network_config: {e}")))?;
        Ok(config)
    }

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
            .with_max_child_host_bytes(
                nonzero(request.max_child_host_bytes)
                    .unwrap_or(self.max_child_host_bytes)
                    .min(self.max_child_host_bytes),
            )
            .with_max_child_memory_bytes(
                nonzero(request.max_child_memory_bytes)
                    .unwrap_or(self.max_child_memory_bytes)
                    .min(self.max_child_memory_bytes),
            )
    }
}

/// Default for how long a signed callback journal can be replayed (7 days).
pub const DEFAULT_JOURNAL_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The environment `name` with content hash `hash`, as journals are bound
/// to it; `None` for an empty `name` (no environment).
fn bound_environment<'a>(
    name: &'a str,
    hash: &'a str,
) -> Option<crate::replay::BoundEnvironment<'a>> {
    (!name.is_empty()).then_some(crate::replay::BoundEnvironment { name, hash })
}

/// Why a request for `language`, which has no pool, is rejected.
fn unserved_language(language: eryx::Language) -> String {
    let feature = match language {
        eryx::Language::JavaScript if !cfg!(feature = "javascript") => Some("javascript"),
        eryx::Language::Shell if !cfg!(feature = "shell") => Some("shell"),
        _ => None,
    };
    match feature {
        Some(feature) => format!(
            "this server was built without {language} support (the `{feature}` cargo feature)"
        ),
        None => format!("this server doesn't serve {language} (see --languages)"),
    }
}

/// Creates a language's sandbox pool on its first request.
pub type PoolFactory = Arc<
    dyn Fn() -> futures::future::BoxFuture<'static, Result<Arc<SandboxPool>, eryx::PoolError>>
        + Send
        + Sync,
>;

/// A language's sandbox pool, created up front or on first use.
enum LanguagePool {
    Ready(Arc<SandboxPool>),
    Lazy {
        pool: tokio::sync::OnceCell<Arc<SandboxPool>>,
        create: PoolFactory,
    },
}

impl LanguagePool {
    /// The pool, created now if this is its first use. A failed creation is
    /// retried on the next request.
    async fn get(&self) -> Result<Arc<SandboxPool>, eryx::PoolError> {
        match self {
            Self::Ready(pool) => Ok(Arc::clone(pool)),
            Self::Lazy { pool, create } => pool.get_or_try_init(|| create()).await.cloned(),
        }
    }
}

impl std::fmt::Debug for LanguagePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(pool) => f.debug_tuple("Ready").field(pool).finish(),
            Self::Lazy { pool, .. } => f.debug_struct("Lazy").field("pool", pool).finish(),
        }
    }
}

/// An environment requests can name, and the pool of sandboxes mounting it.
#[derive(Debug)]
struct EnvironmentPool {
    language: eryx::Language,
    /// Its content hash, which journals recorded under it are bound to.
    hash: String,
    pool: LanguagePool,
}

/// The Eryx gRPC service.
///
/// Holds a shared [`SandboxPool`] per guest language for efficient sandbox
/// reuse across requests.
#[derive(Debug)]
pub struct EryxService {
    pools: HashMap<eryx::Language, LanguagePool>,
    /// The environments requests can name, by name.
    environments: HashMap<String, EnvironmentPool>,
    journal_signer: crate::replay::JournalSigner,
    max_message_size: usize,
    limits: ServerLimits,
    journal_ttl: Duration,
    /// The shell's commands, which `ExecuteRequest.commands` may name;
    /// `None` if not given (see [`Self::with_shell_commands`]).
    shell_commands: Option<Arc<HashSet<String>>>,
}

impl EryxService {
    /// Create a new service instance with the given Python pool and journal
    /// signer. Add pools for other languages with [`Self::with_pool`].
    ///
    /// The `signer` should use a stable key shared across all replicas (see
    /// [`JournalSigner::from_key`](crate::replay::JournalSigner::from_key))
    /// so journals are portable. Use [`Self::new`] for tests/dev where a random
    /// ephemeral key is acceptable.
    #[must_use]
    pub fn with_signer(pool: Arc<SandboxPool>, signer: crate::replay::JournalSigner) -> Self {
        Self {
            pools: HashMap::from([(eryx::Language::Python, LanguagePool::Ready(pool))]),
            environments: HashMap::new(),
            journal_signer: signer,
            max_message_size: MAX_MESSAGE_SIZE,
            limits: ServerLimits::default(),
            journal_ttl: DEFAULT_JOURNAL_TTL,
            shell_commands: None,
        }
    }

    /// Serve `language` from `pool`, replacing any pool it had.
    #[must_use]
    pub fn with_pool(mut self, language: eryx::Language, pool: Arc<SandboxPool>) -> Self {
        self.pools.insert(language, LanguagePool::Ready(pool));
        self
    }

    /// Serve `language` from a pool that `create` makes on the language's
    /// first request, so a language nobody uses costs nothing.
    #[must_use]
    pub fn with_lazy_pool(mut self, language: eryx::Language, create: PoolFactory) -> Self {
        self.pools.insert(
            language,
            LanguagePool::Lazy {
                pool: tokio::sync::OnceCell::new(),
                create,
            },
        );
        self
    }

    /// Serve `environment` under its manifest name
    /// (`ExecuteRequest.environment`), from a pool that `create` makes on its
    /// first request. The pool's sandboxes should mount it
    /// (`SandboxBuilder::with_environment`); requests naming it for another
    /// language are refused, and journals recorded under it are bound to its
    /// name and content hash.
    #[must_use]
    pub fn with_environment_pool(
        mut self,
        environment: &eryx::environment::Environment,
        create: PoolFactory,
    ) -> Self {
        self.environments.insert(
            environment.manifest().name.clone(),
            EnvironmentPool {
                language: environment.language(),
                hash: environment.manifest().content.hash.clone(),
                pool: LanguagePool::Lazy {
                    pool: tokio::sync::OnceCell::new(),
                    create,
                },
            },
        );
        self
    }

    /// Set the ceilings applied to every request's limits. Defaults to
    /// [`ServerLimits::default`].
    #[must_use]
    pub fn with_limits(mut self, limits: ServerLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The shell's command names, so a request's `commands` naming another
    /// is refused up front with `INVALID_ARGUMENT`. Without it, such a
    /// request still fails, when its execution starts.
    #[must_use]
    pub fn with_shell_commands(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.shell_commands = Some(Arc::new(names.into_iter().collect()));
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

        let language = match crate::proto::eryx::v1::Language::try_from(execute_req.language) {
            Ok(
                crate::proto::eryx::v1::Language::Unspecified
                | crate::proto::eryx::v1::Language::Python,
            ) => eryx::Language::Python,
            Ok(crate::proto::eryx::v1::Language::Javascript) => eryx::Language::JavaScript,
            Ok(crate::proto::eryx::v1::Language::Shell) => eryx::Language::Shell,
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "unknown language {}",
                    execute_req.language
                )));
            }
        };
        let environment = execute_req.environment;
        let mut environment_hash = String::new();
        let pool = if environment.is_empty() {
            self.pools
                .get(&language)
                .ok_or_else(|| Status::invalid_argument(unserved_language(language)))?
                .get()
                .await
                .map_err(|e| {
                    Status::unavailable(format!("failed to create the {language} pool: {e}"))
                })?
        } else {
            let served = self.environments.get(&environment).ok_or_else(|| {
                Status::invalid_argument(format!(
                    "this server has no environment {environment:?} (see --environments)"
                ))
            })?;
            if served.language != language {
                return Err(Status::invalid_argument(format!(
                    "environment {environment:?} is for {}, not {language}",
                    served.language
                )));
            }
            environment_hash.clone_from(&served.hash);
            served.pool.get().await.map_err(|e| {
                Status::unavailable(format!(
                    "failed to create the pool for environment {environment:?}: {e}"
                ))
            })?
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
            environment: bound_environment(&environment, &environment_hash),
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
        let (net_config, child_network) = match execute_req.network_config {
            Some(nc) => {
                let (config, child) = self.limits.network_with_children(nc)?;
                (Some(config), child)
            }
            None => (None, eryx::net::ChildNetwork::None),
        };
        if language != eryx::Language::Shell
            && !matches!(child_network, eryx::net::ChildNetwork::None)
        {
            return Err(Status::invalid_argument(format!(
                "network_config.child_network is only for shell requests, not {language}"
            )));
        }
        if language != eryx::Language::Shell && execute_req.allowed_commands.is_some() {
            return Err(Status::invalid_argument(format!(
                "allowed_commands is only for the shell, not {language}"
            )));
        }
        let allowed_commands = execute_req.allowed_commands.map(|allowed| allowed.names);
        if language != eryx::Language::Shell && !execute_req.commands.is_empty() {
            return Err(Status::invalid_argument(format!(
                "commands is only for the shell, not {language}"
            )));
        }
        let command_options = self.limits.command_options(
            execute_req.commands,
            self.shell_commands.as_deref(),
            &child_network,
        )?;

        // Parse and validate secrets, generating their placeholders.
        let secrets = {
            let mut map = HashMap::new();
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
                map.insert(s.name.clone(), secret);
            }
            map
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

        // The secrets' placeholders (`os.environ`, `process.env`, or exported
        // to the shell's children), then the Python builtins injection, so
        // callbacks are reachable from imported modules, and the sys.path
        // setup for supporting files. As a preamble, it doesn't shift the
        // user code's line numbers.
        let library_preamble = match language {
            eryx::Language::Python if vfs_storage.is_some() => {
                format!("{SYS_PATH_INJECT}{BUILTINS_INJECT}")
            }
            eryx::Language::Python => BUILTINS_INJECT.to_owned(),
            _ => String::new(),
        };
        let preamble = eryx::language_preamble(language, &secrets, &library_preamble)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
        let mut sandbox = match pool.acquire().await {
            Ok(s) => s,
            Err(e) => {
                let stats = pool.stats();
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
        let pool_stats = pool.stats();
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
                    language,
                    vfs_storage,
                    state: persist_state.then_some(state_snapshot.as_slice()),
                    enable_tracing,
                    resource_limits,
                    net_config,
                    child_network,
                    secrets,
                    preamble,
                    scrub_stdout,
                    scrub_stderr,
                    scrub_result,
                    result_variable,
                    previous_journal,
                    journal_signer: journal_signer.clone(),
                    journal_ttl,
                    declarations: &declarations,
                    environment: bound_environment(&environment, &environment_hash),
                    max_output_bytes,
                    allowed_commands,
                    command_options,
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
    /// The guest language: only Python gets the (Python) preamble.
    language: eryx::Language,
    /// VFS pre-populated with the request's supporting files, if any.
    vfs_storage: Option<ArcStorage>,
    state: Option<&'a [u8]>,
    enable_tracing: bool,
    resource_limits: ResourceLimits,
    net_config: Option<NetConfig>,
    /// The network for the shell's children, under `net_config`.
    child_network: eryx::net::ChildNetwork,
    secrets: HashMap<String, SecretConfig>,
    /// The secrets' placeholders, then (for Python) the builtins and
    /// sys.path setup.
    preamble: String,
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
    /// The environment the request mounted, which outgoing journals are
    /// also bound to.
    environment: Option<crate::replay::BoundEnvironment<'a>>,
    /// Cap on stdout plus stderr bytes kept and streamed.
    max_output_bytes: usize,
    /// The commands the execution may spawn (shell); `None` allows all.
    allowed_commands: Option<Vec<String>>,
    /// Per-command options for this execution (shell).
    command_options: HashMap<String, eryx::CommandOptions>,
}

/// Longest an execution waits for its trace events to reach the client
/// before sending its result.
const TRACE_RELAY_DRAIN: Duration = Duration::from_secs(1);

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

    // Get the shared Executor and clone the configured callbacks from the sandbox.
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
        let restored = match StateSnapshot::from_bytes(state_snapshot) {
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
    let trace_relay = params.enable_tracing.then(|| {
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
        })
    });

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
    let (net_tx, _net_handler, child_net) = if let Some(config) = params.net_config.clone() {
        let (tx, rx) = mpsc::channel::<NetRequest>(32);
        let allowed_hosts = config.allowed_hosts().len();
        let max_connections = config.max_connections();
        // Validated with the request, so this is only the policy's own
        // guarantee; the handlers spawned above stop once their channels
        // drop.
        let net = ConnectionManager::new(config, params.secrets.clone()).and_then(|manager| {
            // The shell's children share the guest's connection and byte budget.
            let child_net = manager.child_network(params.child_network.clone())?;
            Ok((manager, child_net))
        });
        let (manager, child_net) = match net {
            Ok(net) => net,
            Err(e) => {
                tracing::warn!(error = %e, "invalid network config");
                return ExecuteResult {
                    success: false,
                    error: format!("invalid network config: {e}"),
                    failure_kind: FailureKind::SandboxError as i32,
                    ..Default::default()
                };
            }
        };
        tracing::info!(allowed_hosts, max_connections, "network handler started");
        let handler = tokio::spawn(async move {
            let result = run_net_handler(rx, manager).await;
            tracing::info!("network handler stopped");
            result
        });
        (Some(tx), Some(handler), child_net)
    } else {
        (None, None, None)
    };

    let mut builder = session
        .execute(params.code)
        .with_preamble(params.preamble.as_str())
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
    if let Some(net) = child_net {
        builder = builder.with_child_network(net);
    }
    if let Some(names) = &params.allowed_commands {
        builder = builder.with_allowed_commands(names.iter().cloned());
    }
    for (name, options) in &params.command_options {
        builder = builder.with_command_options(name, options.clone());
    }
    let exec_result = builder.run().await;

    // Wait for callback handler to finish.
    let callback_invocations = callback_handler.await.unwrap_or(0);

    // The execution's trace senders are gone, so the relay ends once it has
    // forwarded what's queued. Let it, so the last events (such as a killed
    // child's `process_end`) reach the client before the ExecuteResult; bounded,
    // in case the client has stopped reading.
    if let Some(relay) = trace_relay {
        let _ = tokio::time::timeout(TRACE_RELAY_DRAIN, relay).await;
    }

    // The output channel sender is dropped once execution completes, so the
    // accumulator task has now (or will shortly) finish draining. Its captured
    // stdout/stderr populate the result on both paths; the copy the guest
    // returns on success is dropped, so the output is held only once.
    let output = output_accumulator
        .await
        .unwrap_or_else(|_| CappedOutput::new(0));
    // The sandbox's own stream budget can also drop output.
    let output_truncated = output.truncated
        || match &exec_result {
            Ok(done) => done.output_truncated,
            Err(failed) => failed.output_truncated(),
        };
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
                environment: params.environment,
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
    // name/args, but the executor still returns ErrorKind::Suspended. Derive the
    // suspension from the error so the outcome is reported as a suspension (with
    // its reason) rather than mis-surfaced as an ordinary sandbox error. There is
    // no prefix journal to resume from in this mode, but the signal is honest.
    let suspended_callback =
        suspended_callback.or_else(|| match exec_result.as_ref().err().map(Error::kind) {
            Some(ErrorKind::Suspended(reason)) => Some(SuspendedCallback {
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
                    skipped_globals = ?snapshot.skipped(),
                    "state snapshot captured"
                );
                (bytes, snapshot.skipped().to_vec())
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
                exit_code: (params.language == eryx::Language::Shell).then_some(0),
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
            let error = if failure_kind == FailureKind::ScriptFailed {
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
                    // There is no `output` on the error path, so read what the
                    // run used from the error. A suspension has none: it
                    // poisons the remaining fuel, so the figure would be
                    // meaningless.
                    peak_memory_bytes: e
                        .stats()
                        .and_then(|stats| stats.peak_memory_bytes)
                        .unwrap_or(0),
                    fuel_consumed: e.stats().and_then(|stats| stats.fuel_consumed).unwrap_or(0),
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
                exit_code: match e.kind() {
                    ErrorKind::ScriptFailed { exit_code, .. } => *exit_code,
                    _ => None,
                },
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
/// [`ErrorKind::ScriptFailed`] is a *script* failure — its traceback is surfaced
/// in `stderr` — so it maps to [`FailureKind::ScriptFailed`].
/// [`ErrorKind::Suspended`] is an expected control outcome (a callback deferred), not
/// a failure, so it maps to [`FailureKind::Suspended`]; callers should branch on
/// the `suspended` flag first. Everything else is a sandbox-level failure whose
/// message belongs in the `error` field.
fn classify_failure(error: &Error) -> FailureKind {
    match error.kind() {
        ErrorKind::ScriptFailed { .. } => FailureKind::ScriptFailed,
        ErrorKind::Timeout(_) => FailureKind::Timeout,
        ErrorKind::FuelExhausted { .. } => FailureKind::FuelExhausted,
        ErrorKind::Cancelled => FailureKind::Cancelled,
        ErrorKind::Suspended(_) => FailureKind::Suspended,
        _ => FailureKind::SandboxError,
    }
}

/// Convert a guest trace event to its proto form, scrubbing secret
/// placeholders from its text fields and context.
///
/// The event JSON uses serde's internally-tagged format with a `"type"` key
/// (`{"type":"line"}`, `{"type":"call","function":"foo"}`, ...); each proto
/// field is read from the key of the same name, and `message` also from
/// `error`. JSON that doesn't parse becomes the event type as is.
fn trace_event(
    req: &TraceRequest,
    secrets: &HashMap<String, SecretConfig>,
) -> crate::proto::eryx::v1::TraceEvent {
    let scrub = |s: &str| {
        if secrets.is_empty() {
            s.to_owned()
        } else {
            scrub_placeholders(s, secrets)
        }
    };
    let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&req.event_json)
    else {
        return crate::proto::eryx::v1::TraceEvent {
            lineno: req.lineno,
            event_type: scrub(&req.event_json),
            context_json: scrub(&req.context_json),
            ..Default::default()
        };
    };
    let text = |key: &str| scrub(map.get(key).and_then(|v| v.as_str()).unwrap_or(""));
    let number = |key: &str| map.get(key).and_then(serde_json::Value::as_u64);
    let event_type = match map.get("type").and_then(|v| v.as_str()) {
        Some(
            kind @ ("line" | "call" | "return" | "exception" | "callback_start" | "callback_end"
            | "process_start" | "process_end"),
        ) => kind.to_owned(),
        _ => "unknown".to_owned(),
    };
    let message = match text("message") {
        message if message.is_empty() => text("error"),
        message => message,
    };
    crate::proto::eryx::v1::TraceEvent {
        lineno: req.lineno,
        event_type,
        function: text("function"),
        message,
        name: text("name"),
        duration_ms: number("duration_ms").unwrap_or(0),
        context_json: scrub(&req.context_json),
        pid: number("pid")
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0),
        exit_code: map
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
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
    fn trace_context_is_forwarded_and_scrubbed() {
        let secret = SecretConfig::new("real", vec![]);
        let placeholder = secret.placeholder().to_string();
        let secrets = HashMap::from([("TOKEN".to_string(), secret)]);
        let req = TraceRequest {
            lineno: 0,
            event_json: serde_json::json!({"type": "process_start", "name": "curl", "pid": 1})
                .to_string(),
            context_json: serde_json::json!({"args": ["-H", format!("token: {placeholder}")]})
                .to_string(),
        };
        let event = trace_event(&req, &secrets);
        let context: serde_json::Value = serde_json::from_str(&event.context_json).unwrap();
        assert_eq!(
            context,
            serde_json::json!({"args": ["-H", "token: [REDACTED]"]})
        );
    }

    #[test]
    fn process_events_carry_pid_exit_code_and_error() {
        let event = |json: serde_json::Value| {
            trace_event(
                &TraceRequest {
                    lineno: 0,
                    event_json: json.to_string(),
                    context_json: String::new(),
                },
                &HashMap::new(),
            )
        };
        let exited = event(serde_json::json!({
            "type": "process_end", "name": "ls", "pid": 2, "exit_code": 1, "duration_ms": 7,
        }));
        assert_eq!(
            (exited.event_type.as_str(), exited.name.as_str(), exited.pid),
            ("process_end", "ls", 2)
        );
        assert_eq!((exited.exit_code, exited.duration_ms), (Some(1), 7));
        let killed = event(serde_json::json!({
            "type": "process_end", "name": "ls", "pid": 3, "error": "killed",
        }));
        assert_eq!(
            (killed.exit_code, killed.message.as_str()),
            (None, "killed")
        );
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

    /// `max_child_host_bytes` and `max_child_memory_bytes` default to their
    /// ceilings and are clamped to them.
    #[test]
    fn child_limits_are_clamped() {
        let ceilings = ServerLimits {
            max_child_host_bytes: 1000,
            max_child_memory_bytes: 2000,
            ..Default::default()
        };
        let bytes = |requested: u64| {
            let request = crate::proto::eryx::v1::ResourceLimits {
                max_child_host_bytes: requested,
                max_child_memory_bytes: requested,
                ..Default::default()
            };
            let limits = ceilings.resource_limits(Some(&request));
            (limits.max_child_host_bytes, limits.max_child_memory_bytes)
        };
        assert_eq!(bytes(0), (Some(1000), Some(2000)));
        assert_eq!(bytes(10), (Some(10), Some(10)));
        assert_eq!(bytes(5000), (Some(1000), Some(2000)));
    }

    /// A request's per-command options are checked against the server's
    /// commands and the request's child network, and parsed like it.
    #[test]
    fn command_options_only_take_away() {
        use crate::proto::eryx::v1::{
            ChildNetwork, CommandOptions, NetworkConfig, child_network::Mode,
        };
        let limits = ServerLimits::default();
        let known: HashSet<String> = ["cat".to_owned()].into();
        let with_network = |mode: Mode, narrow: Option<NetworkConfig>| CommandOptions {
            network: Some(ChildNetwork {
                mode: mode as i32,
                narrow: narrow.map(Box::new),
            }),
            ..Default::default()
        };
        let parse = |options: CommandOptions, children: &eryx::net::ChildNetwork| {
            limits.command_options([("cat".to_owned(), options)].into(), Some(&known), children)
        };
        let none = eryx::net::ChildNetwork::None;
        let inherit = eryx::net::ChildNetwork::Inherit;

        assert!(parse(with_network(Mode::None, None), &none).is_ok());
        assert!(parse(with_network(Mode::Inherit, None), &inherit).is_ok());
        let narrow = Some(NetworkConfig::default());
        assert!(parse(with_network(Mode::Narrow, narrow.clone()), &inherit).is_ok());
        for (options, children) in [
            (with_network(Mode::Inherit, None), &none),
            (with_network(Mode::Narrow, narrow.clone()), &none),
            (with_network(Mode::Narrow, None), &inherit),
            (with_network(Mode::Inherit, narrow), &inherit),
        ] {
            let err = parse(options, children).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
            assert!(
                err.message().starts_with("commands[cat].network: "),
                "{err:?}"
            );
        }
        let unknown = limits
            .command_options(
                [("rg".to_owned(), CommandOptions::default())].into(),
                Some(&known),
                &none,
            )
            .unwrap_err();
        assert_eq!(unknown.code(), tonic::Code::InvalidArgument, "{unknown:?}");
        // Without the server's command list, the execution checks names.
        assert!(
            limits
                .command_options(
                    [("rg".to_owned(), CommandOptions::default())].into(),
                    None,
                    &none
                )
                .is_ok()
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
