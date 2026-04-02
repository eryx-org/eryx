//! eryx-server: gRPC server for sandboxed Python execution.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::service::EryxService;
use eryx_server::telemetry::setup_tracing;
use tonic::transport::Server;

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
}

/// Spawn a background task that periodically records pool gauge metrics.
fn spawn_pool_stats_recorder(pool: Arc<SandboxPool>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_tracing()?;

    let args = Args::parse();
    let addr = args.listen_addr.parse()?;

    let pool_config = PoolConfig {
        max_size: args.pool_max_size,
        min_idle: args.pool_min_idle,
        ..Default::default()
    };

    tracing::info!(
        %addr,
        pool_max_size = pool_config.max_size,
        pool_min_idle = pool_config.min_idle,
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

    // Record pool gauge metrics every 5 seconds.
    spawn_pool_stats_recorder(Arc::clone(&pool));

    let service = EryxService::new(pool);

    Server::builder()
        .add_service(EryxServer::new(service))
        .serve(addr)
        .await?;

    opentelemetry::global::shutdown_tracer_provider();

    Ok(())
}
