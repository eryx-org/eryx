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

    /// Path to a pre-compiled runtime (.cwasm) to use instead of the embedded runtime.
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

/// Spawn a background task that periodically evicts idle sandboxes and
/// records pool gauge metrics.
fn spawn_pool_maintenance(pool: Arc<SandboxPool>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let evicted = pool.evict_idle();
            if evicted > 0 {
                tracing::debug!(evicted, "evicted idle sandboxes");
            }
            let stats = pool.stats();
            metrics::gauge!("eryx_sandbox_pool_in_use").set(stats.in_use as f64);
            metrics::gauge!("eryx_sandbox_pool_available").set(stats.available as f64);
            metrics::gauge!("eryx_sandbox_pool_total").set(stats.total as f64);
            metrics::gauge!("eryx_sandbox_pool_max_size").set(pool.config().max_size as f64);
            metrics::counter!("eryx_sandbox_pool_acquisitions_total")
                .absolute(stats.total_acquisitions);
            metrics::counter!("eryx_sandbox_pool_creations_total").absolute(stats.total_creations);
            metrics::counter!("eryx_sandbox_pool_wait_count_total").absolute(stats.wait_count);
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

    let pool_config = PoolConfig {
        max_size: args.pool_max_size,
        min_idle: args.pool_min_idle,
        acquire_timeout: Duration::from_secs(args.pool_acquire_timeout_secs),
        idle_timeout: Duration::from_secs(args.pool_idle_timeout_secs),
    };

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

    let builder = match (&args.runtime_cwasm, &args.stdlib) {
        (Some(cwasm_path), Some(stdlib_path)) => {
            // Explicit stdlib path — no embedded feature needed for stdlib
            // SAFETY: The user is responsible for providing a trusted .cwasm file
            // that was precompiled with a compatible engine configuration.
            unsafe {
                Sandbox::builder()
                    .with_precompiled_file(cwasm_path)
                    .with_python_stdlib(stdlib_path)
            }
        }
        (Some(cwasm_path), None) => {
            // Custom runtime with embedded stdlib
            // SAFETY: The user is responsible for providing a trusted .cwasm file
            // that was precompiled with a compatible engine configuration.
            unsafe {
                Sandbox::builder()
                    .with_precompiled_file(cwasm_path)
                    .with_embedded_stdlib()?
            }
        }
        (None, Some(_)) => {
            return Err("--stdlib requires --runtime-cwasm".into());
        }
        (None, None) => Sandbox::embedded(),
    };

    let pool = SandboxPool::new(builder, pool_config).await?;
    let pool = Arc::new(pool);

    // Evict idle sandboxes and record pool gauge metrics every 5 seconds.
    spawn_pool_maintenance(Arc::clone(&pool));

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
    };
    tracing::info!(
        ?limits,
        journal_ttl_secs = args.journal_ttl_secs,
        "request ceilings"
    );
    let service = EryxService::with_signer(pool, signer)
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
