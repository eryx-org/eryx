//! eryx-server: gRPC server for sandboxed Python execution.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::service::{DEFAULT_JOURNAL_TTL, EryxService, MAX_MESSAGE_SIZE, ServerLimits};
use eryx_server::telemetry::setup_tracing;
use futures::StreamExt;
use tonic::transport::server::{ServerTlsConfig, TcpIncoming};
use tonic::transport::{Certificate, Identity, Server};

/// gRPC server for sandboxed Python execution via eryx.
#[derive(Parser, Debug)]
#[command(
    name = "eryx-server",
    about = "gRPC server for sandboxed Python execution"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "[::1]:50051", env = "ERYX_LISTEN_ADDR")]
    listen_addr: String,

    /// Maximum number of sandboxes in the pool.
    #[arg(long, default_value_t = 10, env = "ERYX_POOL_MAX_SIZE")]
    pool_max_size: usize,

    /// Minimum number of idle sandboxes to keep warm.
    #[arg(long, default_value_t = 1, env = "ERYX_POOL_MIN_IDLE")]
    pool_min_idle: usize,

    /// Seconds a request waits for a free sandbox before failing with
    /// `UNAVAILABLE`.
    #[arg(long, default_value_t = 30, env = "ERYX_POOL_ACQUIRE_TIMEOUT_SECS")]
    pool_acquire_timeout_secs: u64,

    /// Seconds an idle sandbox beyond `--pool-min-idle` is kept before it is
    /// evicted.
    #[arg(long, default_value_t = 300, env = "ERYX_POOL_IDLE_TIMEOUT_SECS")]
    pool_idle_timeout_secs: u64,

    /// Largest gRPC message, in bytes, the server accepts or sends.
    ///
    /// Clients must raise their own limits to match (e.g. grpc-go's
    /// `MaxCallRecvMsgSize`).
    #[arg(long, default_value_t = MAX_MESSAGE_SIZE, env = "ERYX_MAX_MESSAGE_SIZE")]
    max_message_size: usize,

    /// Maximum concurrent RPCs (HTTP/2 streams) per client connection.
    #[arg(long, default_value_t = 200, env = "ERYX_MAX_CONCURRENT_STREAMS")]
    max_concurrent_streams: u32,

    /// Seconds between HTTP/2 keepalive pings sent to each client; 0 disables
    /// them.
    #[arg(long, default_value_t = 60, env = "ERYX_HTTP2_KEEPALIVE_INTERVAL_SECS")]
    http2_keepalive_interval_secs: u64,

    /// Seconds to wait for a keepalive ping acknowledgement before closing the
    /// connection.
    #[arg(long, default_value_t = 20, env = "ERYX_HTTP2_KEEPALIVE_TIMEOUT_SECS")]
    http2_keepalive_timeout_secs: u64,

    /// Address for the Prometheus metrics endpoint.
    #[arg(long, default_value = "0.0.0.0:9090", env = "ERYX_METRICS_ADDR")]
    metrics_addr: SocketAddr,

    /// Guest languages to accept, comma-separated; a request for any other
    /// is rejected. Each gets its own sandbox pool, sized by the pool flags:
    /// Python's is created at startup, the others' on their first request.
    /// Must include python.
    #[arg(
        long,
        env = "ERYX_LANGUAGES",
        value_delimiter = ',',
        default_value = "python,javascript,shell",
        value_parser = |s: &str| s.parse::<eryx::Language>()
    )]
    languages: Vec<eryx::Language>,

    /// Path to a pre-compiled Python runtime (.cwasm) to use instead of the embedded runtime.
    ///
    /// This allows using a custom runtime with additional packages (e.g. numpy, polars)
    /// baked in via `eryx-precompile`.
    #[arg(long, env = "ERYX_RUNTIME_CWASM")]
    runtime_cwasm: Option<PathBuf>,

    /// Path to Python standard library directory.
    ///
    /// Only used with --runtime-cwasm. Overrides the embedded stdlib, allowing
    /// builds without the `embedded` feature to provide stdlib externally.
    #[arg(long, env = "ERYX_STDLIB")]
    stdlib: Option<PathBuf>,

    /// Hex-encoded 32-byte HMAC key for signing callback replay journals.
    ///
    /// All server replicas must share the same key so that journals are portable
    /// across instances. If not set, a random ephemeral key is generated (journals
    /// signed by one process cannot be verified by another or after a restart).
    /// Prefer the environment variable over the flag, so the key stays out of
    /// the process list.
    #[arg(long, env = "ERYX_JOURNAL_SIGNING_KEY", hide_env_values = true)]
    journal_signing_key: Option<String>,

    /// Seconds a signed callback journal stays valid for replay.
    #[arg(long, default_value_t = DEFAULT_JOURNAL_TTL.as_secs(), env = "ERYX_JOURNAL_TTL_SECS")]
    journal_ttl_secs: u64,

    /// Ceiling on a request's execution timeout, in milliseconds. Larger
    /// request values are clamped to it.
    #[arg(long, default_value_t = 300_000, env = "ERYX_MAX_EXECUTION_TIMEOUT_MS")]
    max_execution_timeout_ms: u64,

    /// Ceiling on a request's callback timeout, in milliseconds.
    #[arg(long, default_value_t = 300_000, env = "ERYX_MAX_CALLBACK_TIMEOUT_MS")]
    max_callback_timeout_ms: u64,

    /// Ceiling on a request's guest memory limit, in bytes.
    #[arg(long, default_value_t = 512 * 1024 * 1024, env = "ERYX_MAX_MEMORY_BYTES")]
    max_memory_bytes: u64,

    /// Ceiling on a request's fuel budget. When set, it is also the budget for
    /// requests that set none. Unset means no ceiling.
    #[arg(long, env = "ERYX_MAX_FUEL")]
    max_fuel: Option<u64>,

    /// Ceiling on a request's callback invocation limit.
    #[arg(long, default_value_t = 10_000, env = "ERYX_MAX_CALLBACK_INVOCATIONS")]
    max_callback_invocations: u32,

    /// Ceiling on a request's concurrent network connections.
    #[arg(long, default_value_t = 100, env = "ERYX_MAX_CONNECTIONS")]
    max_connections: u32,

    /// Ceiling on a request's network connect and I/O timeouts, in milliseconds.
    #[arg(long, default_value_t = 300_000, env = "ERYX_MAX_IO_TIMEOUT_MS")]
    max_io_timeout_ms: u64,

    /// Most stdout plus stderr bytes kept and streamed per execution; the rest
    /// is dropped and the result marked `output_truncated`. At most 16 MiB,
    /// the sandbox's own per-execution output stream limit, which also
    /// charges each write a small overhead and sets `output_truncated` too.
    #[arg(long, default_value_t = 16 * 1024 * 1024, env = "ERYX_MAX_OUTPUT_BYTES")]
    max_output_bytes: usize,

    /// Most host files one execution may hold open at once; opens past it
    /// fail with EDQUOT.
    #[arg(long, default_value_t = eryx::vfs::DEFAULT_MAX_OPEN_HOST_FILES, env = "ERYX_MAX_OPEN_HOST_FILES")]
    max_open_host_files: usize,

    /// Most host files all executions together may hold open at once.
    /// Defaults to half the process's soft `RLIMIT_NOFILE`, at least 256.
    #[arg(long, env = "ERYX_MAX_OPEN_HOST_FILES_TOTAL")]
    max_open_host_files_total: Option<usize>,

    /// Let requests set `network_config.allow_plaintext_secrets`, which sends
    /// real secret values over unencrypted connections. Without it, such
    /// requests are rejected and secrets are only substituted over TLS.
    #[arg(long, env = "ERYX_ALLOW_PLAINTEXT_SECRETS")]
    allow_plaintext_secrets: bool,

    /// Path to a PEM-encoded TLS certificate chain.
    ///
    /// Enables TLS for the gRPC transport. Must be provided together with
    /// `--tls-key`. When neither is set, the server listens over plaintext
    /// (the default).
    #[arg(long, env = "ERYX_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Path to the PEM-encoded TLS private key for `--tls-cert`.
    #[arg(long, env = "ERYX_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Path to a PEM-encoded CA bundle used to verify client certificates.
    ///
    /// Setting this enables mutual TLS: clients must present a certificate
    /// signed by one of the CAs in the bundle, otherwise the connection is
    /// rejected. Requires `--tls-cert`/`--tls-key`.
    #[arg(long, env = "ERYX_TLS_CLIENT_CA", requires = "tls_cert")]
    tls_client_ca: Option<PathBuf>,

    /// An external command for the shell, `NAME=PATH`; repeat for more.
    /// `NAME1,NAME2=PATH` registers one multicall binary (e.g. busybox-style)
    /// under several names, compiled once. PATH is a WASI 0.2 or 0.3 command
    /// component (`.wasm`), or one precompiled by this eryx version's
    /// `eryx-precompile compile` (`.cwasm`, trusted like --runtime-cwasm). A
    /// name overrides the embedded coreutils tool of the same name. In the
    /// environment, separate entries with `;`.
    #[arg(
        long = "shell-command",
        env = "ERYX_SHELL_COMMANDS",
        value_delimiter = ';',
        value_parser = parse_shell_command
    )]
    shell_commands: Vec<ShellCommand>,

    /// Let a --shell-command NAME open blind CONNECT tunnels through the
    /// child proxy, for protocols it can't inspect (e.g. git over SSH):
    /// requests through a tunnel get no secret substitution and no HTTP
    /// rules, and hosts with rules are refused one. Repeat for more names; in
    /// the environment, separate them with `,`. Only matters for requests
    /// that give children network (NetworkConfig.child_network).
    #[arg(
        long = "shell-command-tunnels",
        env = "ERYX_SHELL_COMMAND_TUNNELS",
        value_name = "NAME",
        value_delimiter = ','
    )]
    shell_command_tunnels: Vec<String>,

    /// Leave out the shell's embedded coreutils (cat, head, ls, ...), so it
    /// runs only the --shell-command commands.
    #[arg(long, env = "ERYX_NO_SHELL_COREUTILS")]
    no_shell_coreutils: bool,
}

/// One `--shell-command`: the names to register and the component's path.
#[derive(Debug, Clone)]
struct ShellCommand {
    names: Vec<String>,
    path: PathBuf,
}

fn parse_shell_command(value: &str) -> Result<ShellCommand, String> {
    let (names, path) = value
        .split_once('=')
        .ok_or_else(|| format!("expected NAME[,NAME...]=PATH, got {value:?}"))?;
    let names: Vec<String> = names.split(',').map(|n| n.trim().to_owned()).collect();
    if names.iter().any(String::is_empty) || path.is_empty() {
        return Err(format!("expected NAME[,NAME...]=PATH, got {value:?}"));
    }
    Ok(ShellCommand {
        names,
        path: path.into(),
    })
}

/// Load each `--shell-command`, so a bad one fails startup rather than the
/// first shell request. The names in `tunnels` (`--shell-command-tunnels`)
/// get [`allow_tunnels`](eryx::Command::allow_tunnels), and must each be one
/// of the commands' names.
fn load_shell_commands(
    commands: &[ShellCommand],
    tunnels: &[String],
) -> Result<Vec<(Vec<String>, eryx::Command)>, String> {
    if let Some(name) = tunnels
        .iter()
        .find(|name| !commands.iter().any(|c| c.names.contains(name)))
    {
        return Err(format!(
            "--shell-command-tunnels {name}: not a --shell-command name"
        ));
    }
    let mut loaded = Vec::new();
    for ShellCommand { names, path } in commands {
        let precompiled = path.extension().is_some_and(|ext| ext == "cwasm");
        let command = if precompiled {
            // SAFETY: the operator's own file, trusted as --runtime-cwasm is.
            #[allow(unsafe_code)]
            unsafe {
                eryx::Command::from_precompiled_file(path)
            }
        } else {
            eryx::Command::from_file(path)
        };
        let command = command.map_err(|e| {
            format!(
                "--shell-command {}={}: {e}",
                names.join(","),
                path.display()
            )
        })?;
        let (tunnelling, plain): (Vec<String>, Vec<String>) = names
            .iter()
            .cloned()
            .partition(|name| tunnels.contains(name));
        if !tunnelling.is_empty() {
            loaded.push((tunnelling, command.clone().allow_tunnels()));
        }
        if !plain.is_empty() {
            loaded.push((plain, command));
        }
    }
    Ok(loaded)
}

/// Build a [`ServerTlsConfig`] from the configured cert/key/CA paths.
///
/// Returns `Ok(None)` when no certificate is configured (plaintext mode).
/// clap's `requires` wiring guarantees `--tls-key` accompanies `--tls-cert`
/// and that `--tls-client-ca` is only set alongside them, so this only has to
/// load the files and assemble the config.
fn build_tls_config(args: &Args) -> Result<Option<ServerTlsConfig>, Box<dyn std::error::Error>> {
    let (Some(cert_path), Some(key_path)) = (&args.tls_cert, &args.tls_key) else {
        return Ok(None);
    };

    let cert = std::fs::read(cert_path)
        .map_err(|e| format!("failed to read TLS cert {}: {e}", cert_path.display()))?;
    let key = std::fs::read(key_path)
        .map_err(|e| format!("failed to read TLS key {}: {e}", key_path.display()))?;

    let mut tls = ServerTlsConfig::new().identity(Identity::from_pem(cert, key));

    if let Some(ca_path) = &args.tls_client_ca {
        let ca = std::fs::read(ca_path)
            .map_err(|e| format!("failed to read TLS client CA {}: {e}", ca_path.display()))?;
        // Presence of a client CA requires (and verifies) client certificates:
        // `client_auth_optional` defaults to false, so this is required mTLS.
        tls = tls.client_ca_root(Certificate::from_pem(ca));
    }

    Ok(Some(tls))
}

/// The shell's external commands: the --shell-command ones, and whether to
/// keep the embedded coreutils.
#[derive(Clone)]
struct ShellCommands {
    commands: Vec<(Vec<String>, eryx::Command)>,
    coreutils: bool,
}

/// Build the sandbox pool for a language other than Python, from its
/// embedded runtime.
async fn create_language_pool(
    language: eryx::Language,
    config: PoolConfig,
    shell: ShellCommands,
) -> Result<SandboxPool, eryx::PoolError> {
    match language {
        #[cfg(feature = "javascript")]
        eryx::Language::JavaScript => {
            SandboxPool::new(Sandbox::embedded().javascript(), config).await
        }
        #[cfg(feature = "shell")]
        eryx::Language::Shell => {
            let mut builder = Sandbox::embedded().shell();
            if !shell.coreutils {
                builder = builder.without_embedded_commands();
            }
            for (names, command) in shell.commands {
                builder = builder.with_commands(names, command);
            }
            SandboxPool::new(builder, config).await
        }
        other => {
            let _ = (config, shell);
            Err(eryx::PoolError::Creation(format!(
                "this server has no {other} runtime"
            )))
        }
    }
}

/// Spawn a background task that periodically evicts idle sandboxes and
/// records pool gauge metrics.
fn spawn_pool_maintenance(language: eryx::Language, pool: Arc<SandboxPool>) {
    tokio::spawn(async move {
        let language = language.as_str();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let evicted = pool.evict_idle();
            if evicted > 0 {
                tracing::debug!(evicted, language, "evicted idle sandboxes");
            }
            let stats = pool.stats();
            let labels = [("language", language)];
            metrics::gauge!("eryx_sandbox_pool_in_use", &labels).set(stats.in_use as f64);
            metrics::gauge!("eryx_sandbox_pool_available", &labels).set(stats.available as f64);
            metrics::gauge!("eryx_sandbox_pool_total", &labels).set(stats.total as f64);
            metrics::gauge!("eryx_sandbox_pool_max_size", &labels)
                .set(pool.config().max_size as f64);
            metrics::counter!("eryx_sandbox_pool_acquisitions_total", &labels)
                .absolute(stats.total_acquisitions);
            metrics::counter!("eryx_sandbox_pool_creations_total", &labels)
                .absolute(stats.total_creations);
            metrics::counter!("eryx_sandbox_pool_wait_count_total", &labels)
                .absolute(stats.wait_count);
        }
    });
}

/// Wait for the next SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn next_signal(#[cfg(unix)] terminate: &mut tokio::signal::unix::Signal) {
    #[cfg(unix)]
    let terminate_signal = terminate.recv();
    #[cfg(not(unix))]
    let terminate_signal = std::future::pending::<Option<()>>();

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "failed to listen for interrupt; shutting down");
            }
        }
        _ = terminate_signal => {}
    }
}

fn shutdown_signal() -> std::io::Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    Ok(async move {
        next_signal(
            #[cfg(unix)]
            &mut terminate,
        )
        .await;
        tracing::info!("draining active gRPC calls before shutdown; signal again to force exit");

        // Tokio's handlers replace the default disposition for the life of the
        // process, so without this a second signal could not interrupt a drain
        // stuck on an RPC that never finishes.
        tokio::spawn(async move {
            next_signal(
                #[cfg(unix)]
                &mut terminate,
            )
            .await;
            tracing::warn!("received second shutdown signal; exiting without draining");
            std::process::exit(1);
        });
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tracer_provider = setup_tracing()?;

    let args = Args::parse();
    // A zero ceiling would fail (or forbid) every request rather than mean
    // "unlimited", so refuse it up front.
    for (flag, value) in [
        ("--max-execution-timeout-ms", args.max_execution_timeout_ms),
        ("--max-callback-timeout-ms", args.max_callback_timeout_ms),
        ("--max-memory-bytes", args.max_memory_bytes),
        ("--max-fuel", args.max_fuel.unwrap_or(1)),
        (
            "--max-callback-invocations",
            args.max_callback_invocations.into(),
        ),
        ("--max-connections", args.max_connections.into()),
        ("--max-io-timeout-ms", args.max_io_timeout_ms),
        ("--max-output-bytes", args.max_output_bytes as u64),
        ("--max-open-host-files", args.max_open_host_files as u64),
        (
            "--max-open-host-files-total",
            args.max_open_host_files_total.unwrap_or(1) as u64,
        ),
        ("--journal-ttl-secs", args.journal_ttl_secs),
    ] {
        if value == 0 {
            return Err(format!("{flag} must be greater than 0").into());
        }
    }
    if args.max_output_bytes > eryx::STREAM_BYTES_LIMIT {
        return Err(format!(
            "--max-output-bytes must be at most {}, the sandbox's output stream limit",
            eryx::STREAM_BYTES_LIMIT
        )
        .into());
    }
    let addr = args.listen_addr.parse()?;

    let pool_config = PoolConfig::default()
        .with_max_size(args.pool_max_size)
        .with_min_idle(args.pool_min_idle)
        .with_acquire_timeout(Duration::from_secs(args.pool_acquire_timeout_secs))
        .with_idle_timeout(Duration::from_secs(args.pool_idle_timeout_secs));

    tracing::info!(
        %addr,
        pool_max_size = pool_config.max_size,
        pool_min_idle = pool_config.min_idle,
        pool_acquire_timeout = ?pool_config.acquire_timeout,
        pool_idle_timeout = ?pool_config.idle_timeout,
        max_message_size = args.max_message_size,
        max_concurrent_streams = args.max_concurrent_streams,
        http2_keepalive_interval_secs = args.http2_keepalive_interval_secs,
        http2_keepalive_timeout_secs = args.http2_keepalive_timeout_secs,
        runtime_cwasm = ?args.runtime_cwasm,
        stdlib = ?args.stdlib,
        metrics_addr = %args.metrics_addr,
        "starting eryx gRPC server"
    );

    // Install the Prometheus metrics exporter. It starts an HTTP listener
    // that serves /metrics for scraping.
    let prom_builder =
        metrics_exporter_prometheus::PrometheusBuilder::new().with_http_listener(args.metrics_addr);
    prom_builder.install().map_err(|e| {
        format!(
            "failed to install prometheus metrics exporter on {}: {e}",
            args.metrics_addr
        )
    })?;
    tracing::info!(%args.metrics_addr, "prometheus metrics endpoint started");

    // Python's pool is built and prewarmed up front, as before; the other
    // languages' on their first request. --runtime-cwasm and --stdlib
    // configure Python; the other languages run their embedded runtimes.
    let python_builder = match (&args.runtime_cwasm, &args.stdlib) {
        // SAFETY: The user is responsible for providing a trusted .cwasm file
        // that was precompiled with a compatible engine configuration.
        (Some(cwasm_path), Some(stdlib_path)) => unsafe {
            // Explicit stdlib path — no embedded feature needed for stdlib
            Sandbox::builder()
                .with_precompiled_file(cwasm_path)
                .with_python_stdlib(stdlib_path)
        },
        // SAFETY: as above.
        (Some(cwasm_path), None) => unsafe {
            // Custom runtime with embedded stdlib
            Sandbox::builder()
                .with_precompiled_file(cwasm_path)
                .with_embedded_stdlib()?
        },
        (None, Some(_)) => {
            return Err("--stdlib requires --runtime-cwasm".into());
        }
        (None, None) => Sandbox::embedded(),
    };
    if !args.languages.contains(&eryx::Language::Python) {
        return Err("--languages must include python".into());
    }
    let python_pool = Arc::new(SandboxPool::new(python_builder, pool_config.clone()).await?);
    // Evict idle sandboxes and record pool gauge metrics every 5 seconds.
    spawn_pool_maintenance(eryx::Language::Python, Arc::clone(&python_pool));

    let shell = ShellCommands {
        commands: load_shell_commands(&args.shell_commands, &args.shell_command_tunnels)?,
        coreutils: !args.no_shell_coreutils,
    };
    let shell_configured = !shell.commands.is_empty() || !shell.coreutils;
    if shell_configured && !cfg!(feature = "shell") {
        return Err(
            "--shell-command and --no-shell-coreutils need the `shell` cargo feature".into(),
        );
    }
    if shell_configured && !args.languages.contains(&eryx::Language::Shell) {
        return Err(
            "--shell-command and --no-shell-coreutils need the shell in --languages".into(),
        );
    }
    let mut lazy_pools = Vec::new();
    for &language in &args.languages {
        let built_in = match language {
            eryx::Language::Python => continue,
            eryx::Language::JavaScript => cfg!(feature = "javascript"),
            eryx::Language::Shell => cfg!(feature = "shell"),
            _ => false,
        };
        if !built_in {
            tracing::info!(%language, "not serving {language}: built without its cargo feature");
            continue;
        }
        let config = pool_config.clone();
        let shell = shell.clone();
        let create: eryx_server::service::PoolFactory = Arc::new(move || {
            let config = config.clone();
            let shell = shell.clone();
            Box::pin(async move {
                tracing::info!(%language, "creating sandbox pool on first request");
                let pool = create_language_pool(language, config, shell).await?;
                let pool = Arc::new(pool);
                spawn_pool_maintenance(language, Arc::clone(&pool));
                Ok(pool)
            })
        });
        lazy_pools.push((language, create));
    }

    let signer = match &args.journal_signing_key {
        Some(hex_key) => {
            let bytes = hex::decode(hex_key)
                .map_err(|e| format!("ERYX_JOURNAL_SIGNING_KEY must be valid hex: {e}"))?;
            let key: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
                format!(
                    "ERYX_JOURNAL_SIGNING_KEY must be exactly 32 bytes (64 hex chars), got {} bytes",
                    v.len()
                )
            })?;
            tracing::info!("journal signing key loaded from configuration");
            eryx_server::replay::JournalSigner::from_key(key)
        }
        None => {
            tracing::warn!(
                "no ERYX_JOURNAL_SIGNING_KEY configured — using random ephemeral key; \
                 replay journals will not survive restarts or work across replicas"
            );
            eryx_server::replay::JournalSigner::random()
        }
    };
    let limits = ServerLimits {
        max_execution_timeout: Duration::from_millis(args.max_execution_timeout_ms),
        max_callback_timeout: Duration::from_millis(args.max_callback_timeout_ms),
        max_memory_bytes: args.max_memory_bytes,
        max_fuel: args.max_fuel,
        max_callback_invocations: args.max_callback_invocations,
        max_connections: args.max_connections,
        max_io_timeout: Duration::from_millis(args.max_io_timeout_ms),
        max_output_bytes: args.max_output_bytes,
        max_open_host_files: args.max_open_host_files,
        allow_plaintext_secrets: args.allow_plaintext_secrets,
    };
    if let Some(total) = args.max_open_host_files_total {
        eryx::vfs::set_process_max_open_host_files(total);
    }
    tracing::info!(
        ?limits,
        journal_ttl_secs = args.journal_ttl_secs,
        "request ceilings"
    );
    let service = lazy_pools.into_iter().fold(
        EryxService::with_signer(python_pool, signer),
        |service, (language, create)| service.with_lazy_pool(language, create),
    );
    let service = service
        .with_max_message_size(args.max_message_size)
        .with_limits(limits)
        .with_journal_ttl(Duration::from_secs(args.journal_ttl_secs));

    // The overall ("") status starts as SERVING; the pool is already warm here.
    // Once shutdown starts the listener closes, so new probes fail to connect.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<EryxServer<EryxService>>()
        .await;

    let mut server = Server::builder()
        .max_concurrent_streams(args.max_concurrent_streams)
        .http2_keepalive_interval(
            (args.http2_keepalive_interval_secs > 0)
                .then(|| Duration::from_secs(args.http2_keepalive_interval_secs)),
        )
        .http2_keepalive_timeout(Some(Duration::from_secs(args.http2_keepalive_timeout_secs)));
    match build_tls_config(&args)? {
        Some(tls) => {
            let mtls = args.tls_client_ca.is_some();
            server = server.tls_config(tls)?;
            tracing::info!(mtls, "TLS enabled for gRPC transport");
        }
        None => {
            tracing::info!("TLS disabled — serving gRPC over plaintext");
        }
    }

    // Tonic keeps its listener open until every connection drains, so the
    // kernel would keep completing handshakes nobody serves. End the incoming
    // stream on shutdown instead, which drops the listener, and only then let
    // tonic start draining.
    let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel::<()>();
    let listener = TcpIncoming::bind(addr)?.with_nodelay(Some(true));
    let incoming = futures::stream::unfold(
        (listener, Box::pin(shutdown_signal()?), stopped_tx),
        |(mut listener, mut shutdown, stopped_tx)| async move {
            tokio::select! {
                conn = listener.next() => Some((conn?, (listener, shutdown, stopped_tx))),
                () = &mut shutdown => None,
            }
        },
    );

    server
        .add_service(health_service)
        .add_service(service.into_server())
        .serve_with_incoming_shutdown(incoming, async {
            let _ = stopped_rx.await;
        })
        .await?;

    if let Some(provider) = tracer_provider
        && let Err(e) = provider.shutdown()
    {
        eprintln!("failed to shut down tracer provider: {e}");
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_flags_parse() {
        let one = parse_shell_command("rg=/opt/rg.wasm").unwrap();
        assert_eq!(one.names, ["rg"]);
        assert_eq!(one.path, PathBuf::from("/opt/rg.wasm"));
        let multi = parse_shell_command("cat,head=/opt/coreutils.cwasm").unwrap();
        assert_eq!(multi.names, ["cat", "head"]);

        for bad in ["rg", "=/opt/rg.wasm", "rg=", "a,,b=/x.wasm"] {
            assert!(parse_shell_command(bad).is_err(), "{bad}");
        }

        let args = Args::try_parse_from([
            "eryx-server",
            "--shell-command",
            "rg=/opt/rg.wasm",
            "--shell-command",
            "cat,head=/opt/coreutils.cwasm",
            "--no-shell-coreutils",
        ])
        .unwrap();
        assert_eq!(args.shell_commands.len(), 2);
        assert!(args.no_shell_coreutils);
    }

    /// `--shell-command` registers a command (here coreutils' multicall
    /// binary, as `cat` only) and `--no-shell-coreutils` drops the rest.
    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_command_flags_shape_the_shell_pool() {
        let coreutils =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eryx-runtime/coreutils.cwasm");
        let shell = ShellCommands {
            commands: load_shell_commands(
                &[ShellCommand {
                    names: vec!["cat".into()],
                    path: coreutils,
                }],
                &[],
            )
            .unwrap(),
            coreutils: false,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox
            .execute("echo hi | cat; sort; echo \"sort=$?\"")
            .await
            .unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "hi\nsort=127\n");

        // With the default flags, the embedded coreutils are all there.
        let shell = ShellCommands {
            commands: Vec::new(),
            coreutils: true,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox.execute("printf 'b\\na\\n' | sort").await.unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "a\nb\n");
    }

    /// `--shell-command-tunnels` splits a multicall entry by name, and must
    /// name a `--shell-command`.
    #[test]
    fn shell_command_tunnels_name_registered_commands() {
        let coreutils = ShellCommand {
            names: vec!["cat".into(), "head".into()],
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eryx-runtime/coreutils.cwasm"),
        };
        let loaded =
            load_shell_commands(std::slice::from_ref(&coreutils), &["head".into()]).unwrap();
        let names: Vec<_> = loaded.iter().map(|(names, _)| names.clone()).collect();
        assert_eq!(names, [vec!["head".to_string()], vec!["cat".to_string()]]);

        let err = load_shell_commands(&[coreutils], &["curl".into()]).unwrap_err();
        assert_eq!(
            err,
            "--shell-command-tunnels curl: not a --shell-command name"
        );
    }

    #[test]
    fn a_missing_shell_command_fails_startup_clearly() {
        let err = load_shell_commands(
            &[ShellCommand {
                names: vec!["rg".into()],
                path: "/nonexistent/rg.wasm".into(),
            }],
            &[],
        )
        .unwrap_err();
        assert!(
            err.starts_with("--shell-command rg=/nonexistent/rg.wasm: "),
            "{err}"
        );
    }
}
