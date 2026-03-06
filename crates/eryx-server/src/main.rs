//! eryx-server: gRPC server for sandboxed Python execution.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::service::EryxService;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,h2=warn,tonic::transport=warn")),
        )
        .with(tracing_logfmt::layer())
        .init();

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
        "starting eryx gRPC server"
    );

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
    let service = EryxService::new(Arc::new(pool));

    Server::builder()
        .add_service(EryxServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
