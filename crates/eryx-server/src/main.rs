//! eryx-server: gRPC server for sandboxed Python execution.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use eryx::{PoolConfig, Sandbox, SandboxBuilder, SandboxPool};
use eryx_server::mounts::MountLimits;
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

    /// Directory of environments that requests can mount by name
    /// (`ExecuteRequest.environment`): one per subdirectory, each packed with
    /// `eryx-precompile env pack` and named by its manifest. They're loaded
    /// and checked at startup; one that fails to load, has no name, shares
    /// another's name, or is for a language the server doesn't serve stops
    /// the server. Symlinks are followed and `.` entries skipped. Each
    /// gets its own sandbox pool, sized by the pool flags and created on its
    /// first request.
    #[arg(long, env = "ERYX_ENVIRONMENTS")]
    environments: Option<PathBuf>,

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

    /// Path to the site-packages directory --runtime-cwasm was
    /// pre-initialized with (`eryx-precompile compile --preinit
    /// --site-packages DIR`), mounted read-only at `/site-packages` as it was
    /// then, for Python sandboxes and --shell-python. Without it, only the
    /// modules imported during pre-initialization exist, and any a package
    /// imports later (e.g. `numpy.rec` for pandas' `describe()`) fails with
    /// `ModuleNotFoundError`. Only used with --runtime-cwasm.
    #[arg(long, env = "ERYX_SITE_PACKAGES")]
    site_packages: Option<PathBuf>,

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

    /// Ceiling on `resource_limits.max_child_host_bytes`: the most host
    /// memory a shell execution's child processes' arguments and
    /// environments may hold at once. Also the value for requests that set
    /// none.
    #[arg(long, default_value_t = 64 * 1024 * 1024, env = "ERYX_MAX_CHILD_HOST_BYTES")]
    max_child_host_bytes: u64,

    /// Ceiling on `resource_limits.max_child_memory_bytes`: the most memory
    /// a shell execution's running child processes may hold together, on top
    /// of its own `--max-memory-bytes`. Also the value for requests that set
    /// none. Raising it raises the host memory one execution can use.
    #[arg(long, default_value_t = 256 * 1024 * 1024, env = "ERYX_MAX_CHILD_MEMORY_BYTES")]
    max_child_memory_bytes: u64,

    /// Most mounts (`ExecuteRequest.mounts`) one request may have.
    #[arg(long, default_value_t = MountLimits::default().max_mounts, env = "ERYX_MAX_MOUNTS")]
    max_mounts: usize,

    /// Most file operations (`FsRequest`s) one execution may send its
    /// client for its mounts; past it, the code's file operations on them
    /// fail.
    #[arg(long, default_value_t = MountLimits::default().max_ops, env = "ERYX_MAX_MOUNT_OPS")]
    max_mount_ops: u32,

    /// Most bytes of `FsResponse`s a client may send one execution.
    #[arg(long, default_value_t = MountLimits::default().max_transfer_bytes, env = "ERYX_MAX_MOUNT_TRANSFER_BYTES")]
    max_mount_transfer_bytes: usize,

    /// Most host memory one execution's cache of mounted file data may
    /// hold, evicted oldest first. A read that can't fit fails.
    #[arg(long, default_value_t = MountLimits::default().max_cache_bytes, env = "ERYX_MAX_MOUNT_CACHE_BYTES")]
    max_mount_cache_bytes: u64,

    /// Most host memory one execution's directory listings from its mounts
    /// may hold; they're kept for the execution.
    #[arg(long, default_value_t = MountLimits::default().max_listing_bytes, env = "ERYX_MAX_MOUNT_LISTING_BYTES")]
    max_mount_listing_bytes: u64,

    /// Most entries one directory's listing on a mount may have.
    #[arg(long, default_value_t = MountLimits::default().max_dir_entries, env = "ERYX_MAX_MOUNT_DIR_ENTRIES")]
    max_mount_dir_entries: usize,

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

    /// An external command for the shell, `NAME=PATH[@sha256:<hex>]`; repeat
    /// for more. `NAME1,NAME2=PATH` registers one multicall binary (e.g.
    /// busybox-style) under several names, compiled once. PATH is a WASI 0.2
    /// or 0.3 command component or a WASI preview 1 command module (`.wasm`),
    /// or one precompiled by this eryx version's `eryx-precompile compile`
    /// (`.cwasm`, trusted like --runtime-cwasm). A name overrides the embedded
    /// coreutils tool of the same name. A `.wasm` is compiled once into
    /// eryx's cache directory, keyed by its content, so a restart reuses it.
    ///
    /// `@sha256:<hex>` pins a `.wasm` to that digest: startup fails if the
    /// file differs. In the environment, separate entries with `;`; empty
    /// entries, and an empty ERYX_SHELL_COMMANDS, mean no commands.
    #[arg(
        long = "shell-command",
        env = "ERYX_SHELL_COMMANDS",
        value_name = "NAME=PATH[@sha256:<hex>]",
        value_delimiter = ';'
    )]
    shell_commands: Vec<String>,

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

    /// Let a --shell-command NAME run commands itself (through
    /// `eryx:process/run`), as git runs `git` for `stash`: nested commands
    /// share the execution's commands, network and limits. Repeat for more
    /// names; in the environment, separate them with `,`. A request can turn
    /// it off for one execution (ExecuteRequest.commands), not on.
    #[arg(
        long = "shell-command-spawn",
        env = "ERYX_SHELL_COMMAND_SPAWN",
        value_name = "NAME",
        value_delimiter = ','
    )]
    shell_command_spawn: Vec<String>,

    /// Leave out the shell's embedded commands (coreutils: cat, head, ls,
    /// ..., and sh/bash for commands that spawn them), so it runs only the
    /// --shell-command commands (and python, with --shell-python).
    #[arg(long, env = "ERYX_NO_SHELL_EMBEDDED_COMMANDS")]
    no_shell_embedded_commands: bool,

    /// Let the shell run Python as `python` and `python3` (`python3 -c ...`,
    /// `python3 script.py`, `... | python3`): a fresh interpreter per run,
    /// from the server's Python runtime (--runtime-cwasm and --stdlib, or the
    /// embedded one), sharing the script's files, env and limits. It has no
    /// callbacks or network.
    #[arg(long, env = "ERYX_SHELL_PYTHON")]
    shell_python: bool,

    /// Let the shell run JavaScript as `node` and `js` (`node -e ...`,
    /// `node script.js`, `... | node`): a fresh QuickJS instance per run,
    /// from the embedded JavaScript runtime, with Node's `process`,
    /// `require` and `node:fs` on the script's files, env and limits. It has
    /// no callbacks or network.
    #[arg(long, env = "ERYX_SHELL_NODE")]
    shell_node: bool,
}

/// One `--shell-command`: the names to register, the component's path, and
/// the digest it's pinned to, if any.
#[derive(Debug, Clone)]
struct ShellCommand {
    names: Vec<String>,
    path: PathBuf,
    /// `sha256:<hex>`, from `PATH@sha256:<hex>`.
    sha256: Option<String>,
}

/// Parse the `--shell-command` values, skipping empty ones, so an empty
/// ERYX_SHELL_COMMANDS (or a stray `;`) means no commands.
fn parse_shell_commands(values: &[String]) -> Result<Vec<ShellCommand>, String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(parse_shell_command)
        .collect()
}

fn parse_shell_command(value: &str) -> Result<ShellCommand, String> {
    let invalid =
        || format!("--shell-command: expected NAME[,NAME...]=PATH[@sha256:<hex>], got {value:?}");
    let (names, path) = value.split_once('=').ok_or_else(invalid)?;
    let names: Vec<String> = names.split(',').map(|n| n.trim().to_owned()).collect();
    // The pin is the last `@sha256:`, so a path may contain `@`.
    let (path, sha256) = match path.rsplit_once("@sha256:") {
        Some((path, hex)) => (path, Some(format!("sha256:{hex}"))),
        None => (path, None),
    };
    if names.iter().any(String::is_empty) || path.is_empty() {
        return Err(invalid());
    }
    Ok(ShellCommand {
        names,
        path: path.into(),
        sha256,
    })
}

/// Load each `--shell-command`, so a bad one fails startup rather than the
/// first shell request. The names in `tunnels` (`--shell-command-tunnels`)
/// get [`allow_tunnels`](eryx::Command::allow_tunnels), and those in `spawn`
/// (`--shell-command-spawn`) [`allow_spawn`](eryx::Command::allow_spawn);
/// each must be one of the commands' names.
fn load_shell_commands(
    commands: &[ShellCommand],
    tunnels: &[String],
    spawn: &[String],
) -> Result<Vec<(Vec<String>, eryx::Command)>, String> {
    for (flag, names) in [("tunnels", tunnels), ("spawn", spawn)] {
        if let Some(name) = names
            .iter()
            .find(|name| !commands.iter().any(|c| c.names.contains(name)))
        {
            return Err(format!(
                "--shell-command-{flag} {name}: not a --shell-command name"
            ));
        }
    }
    let mut loaded = Vec::new();
    for ShellCommand {
        names,
        path,
        sha256,
    } in commands
    {
        let flag = || {
            let pin = sha256
                .as_deref()
                .map(|d| format!("@{d}"))
                .unwrap_or_default();
            format!(
                "--shell-command {}={}{pin}",
                names.join(","),
                path.display()
            )
        };
        let precompiled = path.extension().is_some_and(|ext| ext == "cwasm");
        let command = match (precompiled, sha256) {
            // A pin is for the `.wasm` eryx compiles itself; a `.cwasm` is
            // trusted as a whole, like --runtime-cwasm.
            (true, Some(_)) => {
                return Err(format!(
                    "{}: only a .wasm can be pinned; pin the .wasm and let eryx compile it",
                    flag()
                ));
            }
            // SAFETY: the operator's own file, trusted as --runtime-cwasm is.
            #[allow(unsafe_code)]
            (true, None) => unsafe { eryx::Command::from_precompiled_file(path) },
            // Pinned or not, compiled once into eryx's cache, which is keyed
            // by the file's content, so a changed file is compiled afresh.
            (false, sha256) => eryx::Command::load_cached(path, sha256.as_deref()),
        };
        let command = command.map_err(|e| format!("{}: {e}", flag()))?;
        // One registration per combination of grants among its names.
        for grants in [(true, true), (true, false), (false, true), (false, false)] {
            let granted: Vec<String> = names
                .iter()
                .filter(|name| (tunnels.contains(name), spawn.contains(name)) == grants)
                .cloned()
                .collect();
            if granted.is_empty() {
                continue;
            }
            let mut command = command.clone();
            if grants.0 {
                command = command.allow_tunnels();
            }
            if grants.1 {
                command = command.allow_spawn();
            }
            loaded.push((granted, command));
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

/// The shell's external commands: the --shell-command ones (and python),
/// and whether to keep the embedded ones.
#[derive(Clone)]
struct ShellCommands {
    commands: Vec<(Vec<String>, eryx::Command)>,
    embedded: bool,
}

/// `python`/`python3` for the shell (--shell-python): the server's Python
/// runtime, `runtime_cwasm` with `stdlib` (the embedded stdlib if `None`) and
/// `site_packages`, or the embedded one.
fn python_command(python: &PythonRuntime) -> Result<eryx::Command, String> {
    let fail = |e: eryx::Error| format!("--shell-python: {e}");
    let command = match &python.runtime_cwasm {
        Some(path) => {
            let stdlib = match &python.stdlib {
                Some(stdlib) => stdlib.clone(),
                None => eryx::embedded::EmbeddedResources::get()
                    .map_err(fail)?
                    .stdlib()
                    .to_path_buf(),
            };
            // SAFETY: --runtime-cwasm is trusted, as for the Python pool.
            unsafe {
                eryx::Command::guest_from_precompiled_file(
                    eryx::Language::Python,
                    path,
                    Some(stdlib),
                )
            }
            .map_err(fail)?
        }
        None => eryx::Command::guest(eryx::Language::Python).map_err(fail)?,
    };
    match &python.site_packages {
        Some(dir) => command.with_site_packages(dir).map_err(fail),
        None => Ok(command),
    }
}

/// `node`/`js` for the shell (--shell-node): the embedded JavaScript runtime.
fn node_command() -> Result<eryx::Command, String> {
    #[cfg(feature = "javascript")]
    return eryx::Command::guest(eryx::Language::JavaScript)
        .map_err(|e| format!("--shell-node: {e}"));
    #[cfg(not(feature = "javascript"))]
    Err("--shell-node needs the `javascript` cargo feature".to_owned())
}

/// Build the sandbox pool for a language other than Python, from its
/// embedded runtime.
async fn create_language_pool(
    language: eryx::Language,
    config: PoolConfig,
    shell: ShellCommands,
    python: PythonRuntime,
    environment: Option<eryx::environment::Environment>,
) -> Result<SandboxPool, eryx::PoolError> {
    /// `builder`, mounting `environment` if there is one.
    fn mount<L: eryx::language::GuestLanguage>(
        builder: SandboxBuilder<L, eryx::state::Has>,
        environment: Option<&eryx::environment::Environment>,
    ) -> SandboxBuilder<L, eryx::state::Has> {
        match environment {
            Some(environment) => builder.with_environment(environment),
            None => builder,
        }
    }
    let environment = environment.as_ref();
    match language {
        eryx::Language::Python => {
            let builder = python
                .builder()
                .map_err(|e| eryx::PoolError::Creation(e.to_string()))?;
            SandboxPool::new(mount(builder, environment), config).await
        }
        #[cfg(feature = "javascript")]
        eryx::Language::JavaScript => {
            SandboxPool::new(mount(Sandbox::embedded().javascript(), environment), config).await
        }
        #[cfg(feature = "shell")]
        eryx::Language::Shell => {
            let mut builder = Sandbox::embedded().shell();
            if !shell.embedded {
                builder = builder.without_embedded_commands();
            }
            for (names, command) in shell.commands {
                builder = builder.with_commands(names, command);
            }
            SandboxPool::new(mount(builder, environment), config).await
        }
        other => {
            let _ = (config, shell);
            Err(eryx::PoolError::Creation(format!(
                "this server has no {other} runtime"
            )))
        }
    }
}

/// Python's runtime: --runtime-cwasm, --stdlib and --site-packages.
#[derive(Clone, Default)]
struct PythonRuntime {
    runtime_cwasm: Option<PathBuf>,
    stdlib: Option<PathBuf>,
    site_packages: Option<PathBuf>,
}

impl PythonRuntime {
    /// A builder for Python sandboxes on this runtime.
    fn builder(
        &self,
    ) -> Result<SandboxBuilder<eryx::language::Python, eryx::state::Has>, Box<dyn std::error::Error>>
    {
        if self.runtime_cwasm.is_none() && self.site_packages.is_some() {
            return Err("--site-packages requires --runtime-cwasm".into());
        }
        let builder = match (&self.runtime_cwasm, &self.stdlib) {
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
        Ok(match &self.site_packages {
            Some(dir) => builder.with_site_packages(dir),
            None => builder,
        })
    }
}

/// The environments in `dir`, one per subdirectory (or symlink to one, as
/// Kubernetes volumes have; entries starting with `.` are skipped), refusing
/// one without a name, two with the same name, or one for a language not in
/// `languages`, the languages the server serves.
fn load_environments(
    dir: &std::path::Path,
    languages: &[eryx::Language],
) -> Result<Vec<eryx::environment::Environment>, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    for entry in
        std::fs::read_dir(dir).map_err(|e| format!("--environments {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // Follows symlinks, unlike `entry.file_type()`.
        if std::fs::metadata(&path)?.is_dir() {
            paths.push(path);
        } else {
            tracing::info!(path = %path.display(), "--environments: skipping a file that isn't a directory");
        }
    }
    paths.sort();
    let mut names = std::collections::HashMap::new();
    let mut environments = Vec::new();
    for path in paths {
        let environment = eryx::environment::Environment::load(&path)
            .map_err(|e| format!("environment {}: {e}", path.display()))?;
        let name = environment.manifest().name.clone();
        if name.is_empty() {
            return Err(format!(
                "environment {} has no name, so no request could select it",
                path.display()
            )
            .into());
        }
        if let Some(first) = names.insert(name.clone(), path.clone()) {
            return Err(format!(
                "environments {} and {} are both named {name:?}",
                first.display(),
                path.display()
            )
            .into());
        }
        let language = environment.language();
        if !languages.contains(&language) {
            return Err(format!(
                "environment {name:?} ({}) is for {language}, which this server doesn't serve \
                 (see --languages and the server's cargo features)",
                path.display()
            )
            .into());
        }
        environments.push(environment);
    }
    Ok(environments)
}

/// A pool for `language`, mounting `environment` if given, that's created
/// on its first request.
fn lazy_pool(
    language: eryx::Language,
    environment: Option<eryx::environment::Environment>,
    config: &PoolConfig,
    shell: &ShellCommands,
    python: &PythonRuntime,
) -> eryx_server::service::PoolFactory {
    let (config, shell, python) = (config.clone(), shell.clone(), python.clone());
    Arc::new(move || {
        let (config, shell, python, environment) = (
            config.clone(),
            shell.clone(),
            python.clone(),
            environment.clone(),
        );
        Box::pin(async move {
            let name = environment.as_ref().map(|e| e.manifest().name.clone());
            tracing::info!(%language, environment = name, "creating sandbox pool on first request");
            let pool = create_language_pool(language, config, shell, python, environment).await?;
            let pool = Arc::new(pool);
            spawn_pool_maintenance(language, name, Arc::clone(&pool));
            Ok(pool)
        })
    })
}

/// Spawn a background task that periodically evicts idle sandboxes and
/// records pool gauge metrics, labelled with the pool's language and
/// environment (empty for none).
fn spawn_pool_maintenance(
    language: eryx::Language,
    environment: Option<String>,
    pool: Arc<SandboxPool>,
) {
    tokio::spawn(async move {
        let language = language.as_str();
        let environment = environment.unwrap_or_default();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let evicted = pool.evict_idle();
            if evicted > 0 {
                tracing::debug!(evicted, language, "evicted idle sandboxes");
            }
            let stats = pool.stats();
            let labels = [
                ("language", language.to_owned()),
                ("environment", environment.clone()),
            ];
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
        ("--max-child-host-bytes", args.max_child_host_bytes),
        ("--max-child-memory-bytes", args.max_child_memory_bytes),
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
        site_packages = ?args.site_packages,
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
    // languages' on their first request. --runtime-cwasm, --stdlib and
    // --site-packages configure Python; the other languages run their
    // embedded runtimes.
    let python = PythonRuntime {
        runtime_cwasm: args.runtime_cwasm.clone(),
        stdlib: args.stdlib.clone(),
        site_packages: args.site_packages.clone(),
    };
    let python_builder = python.builder()?;
    if !args.languages.contains(&eryx::Language::Python) {
        return Err("--languages must include python".into());
    }
    let python_pool = Arc::new(SandboxPool::new(python_builder, pool_config.clone()).await?);
    // Evict idle sandboxes and record pool gauge metrics every 5 seconds.
    spawn_pool_maintenance(eryx::Language::Python, None, Arc::clone(&python_pool));

    let mut commands = load_shell_commands(
        &parse_shell_commands(&args.shell_commands)?,
        &args.shell_command_tunnels,
        &args.shell_command_spawn,
    )?;
    if args.shell_python {
        commands.push((
            vec!["python".to_owned(), "python3".to_owned()],
            python_command(&python)?,
        ));
    }
    if args.shell_node {
        commands.push((vec!["node".to_owned(), "js".to_owned()], node_command()?));
    }
    let shell = ShellCommands {
        commands,
        embedded: !args.no_shell_embedded_commands,
    };
    // What requests' ExecuteRequest.commands may name.
    let shell_command_names: Vec<String> = shell
        .commands
        .iter()
        .flat_map(|(names, _)| names.iter().cloned())
        .chain(
            shell
                .embedded
                // With the embedded shell as `sh`/`bash`, for commands that
                // spawn them.
                .then(|| {
                    eryx::embedded::COREUTILS_TOOLS
                        .iter()
                        .chain(&["sh", "bash"])
                })
                .into_iter()
                .flatten()
                .map(|tool| (*tool).to_owned()),
        )
        .collect();
    let shell_configured = !shell.commands.is_empty() || !shell.embedded;
    if shell_configured && !cfg!(feature = "shell") {
        return Err(
            "--shell-command, --shell-python, --shell-node and --no-shell-embedded-commands need \
             the `shell` cargo feature"
                .into(),
        );
    }
    if shell_configured && !args.languages.contains(&eryx::Language::Shell) {
        return Err(
            "--shell-command, --shell-python, --shell-node and --no-shell-embedded-commands need \
             the shell in --languages"
                .into(),
        );
    }
    let mut lazy_pools = Vec::new();
    // The languages this server serves: those in --languages it was built with.
    let mut served = vec![eryx::Language::Python];
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
        served.push(language);
        lazy_pools.push((
            language,
            lazy_pool(language, None, &pool_config, &shell, &python),
        ));
    }
    let environment_pools = match &args.environments {
        Some(dir) => load_environments(dir, &served)?,
        None => Vec::new(),
    }
    .into_iter()
    .map(|environment| {
        tracing::info!(
            name = environment.manifest().name,
            language = %environment.language(),
            hash = environment.manifest().content.hash,
            root = %environment.root().display(),
            "serving environment"
        );
        let create = lazy_pool(
            environment.language(),
            Some(environment.clone()),
            &pool_config,
            &shell,
            &python,
        );
        (environment, create)
    })
    .collect::<Vec<_>>();

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
        max_child_host_bytes: args.max_child_host_bytes,
        max_child_memory_bytes: args.max_child_memory_bytes,
        allow_plaintext_secrets: args.allow_plaintext_secrets,
        mounts: MountLimits {
            max_mounts: args.max_mounts,
            max_ops: args.max_mount_ops,
            max_transfer_bytes: args.max_mount_transfer_bytes,
            max_cache_bytes: args.max_mount_cache_bytes,
            max_listing_bytes: args.max_mount_listing_bytes,
            max_dir_entries: args.max_mount_dir_entries,
        },
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
    let service = environment_pools
        .into_iter()
        .fold(service, |service, (environment, create)| {
            service.with_environment_pool(&environment, create)
        });
    let service = service
        .with_max_message_size(args.max_message_size)
        .with_limits(limits)
        .with_shell_commands(shell_command_names)
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
        assert_eq!(one.sha256, None);
        let pinned = parse_shell_command("rg=/opt/a@b/rg.wasm@sha256:00ff").unwrap();
        assert_eq!(pinned.path, PathBuf::from("/opt/a@b/rg.wasm"));
        assert_eq!(pinned.sha256.as_deref(), Some("sha256:00ff"));
        let multi = parse_shell_command("cat,head=/opt/coreutils.cwasm").unwrap();
        assert_eq!(multi.names, ["cat", "head"]);

        for bad in [
            "rg",
            "=/opt/rg.wasm",
            "rg=",
            "a,,b=/x.wasm",
            "rg=@sha256:00",
        ] {
            assert!(parse_shell_command(bad).is_err(), "{bad}");
        }

        let args = Args::try_parse_from([
            "eryx-server",
            "--shell-command",
            "rg=/opt/rg.wasm",
            "--shell-command",
            "cat,head=/opt/coreutils.cwasm",
            "--no-shell-embedded-commands",
            "--shell-python",
            "--shell-node",
        ])
        .unwrap();
        assert_eq!(parse_shell_commands(&args.shell_commands).unwrap().len(), 2);
        assert!(args.no_shell_embedded_commands);
        assert!(args.shell_python);
        assert!(args.shell_node);
    }

    /// `--shell-command` registers a command (here coreutils' multicall
    /// binary, as `cat` only) and `--no-shell-embedded-commands` drops the
    /// rest; `--shell-python` adds python.
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
                    sha256: None,
                }],
                &[],
                &[],
            )
            .unwrap(),
            embedded: false,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
            PythonRuntime::default(),
            None,
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
            embedded: true,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
            PythonRuntime::default(),
            None,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox.execute("printf 'b\\na\\n' | sort").await.unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "a\nb\n");
    }

    /// `--shell-python` registers the embedded Python as `python` and
    /// `python3`.
    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_python_runs_python_in_the_shell() {
        let shell = ShellCommands {
            commands: vec![(
                vec!["python".into(), "python3".into()],
                python_command(&PythonRuntime::default()).unwrap(),
            )],
            embedded: true,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
            PythonRuntime::default(),
            None,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox
            .execute("seq 3 | python3 -c 'import sys; print(sum(map(int, sys.stdin)))'")
            .await
            .unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "6\n");
    }

    /// `--site-packages` mounts the directory for the Python pool and for
    /// `--shell-python`, so modules the snapshot didn't import load.
    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn site_packages_are_mounted_for_python_and_shell_python() {
        let site = tempfile::tempdir().unwrap();
        std::fs::write(site.path().join("lazy_mod.py"), "VALUE = 42\n").unwrap();
        let python = PythonRuntime {
            runtime_cwasm: Some(
                eryx::embedded::runtime_path(eryx::Language::Python)
                    .unwrap()
                    .unwrap()
                    .to_path_buf(),
            ),
            stdlib: None,
            site_packages: Some(site.path().to_path_buf()),
        };

        let pool = create_language_pool(
            eryx::Language::Python,
            PoolConfig::default().with_max_size(1),
            ShellCommands {
                commands: Vec::new(),
                embedded: true,
            },
            python.clone(),
            None,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox
            .execute("import lazy_mod; print(lazy_mod.VALUE)")
            .await
            .unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "42\n");

        let shell = ShellCommands {
            commands: vec![(vec!["python3".into()], python_command(&python).unwrap())],
            embedded: true,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
            python,
            None,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox
            .execute("python3 -c 'import lazy_mod; print(lazy_mod.VALUE)'")
            .await
            .unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "42\n");
    }

    /// `--site-packages` without `--runtime-cwasm` is refused.
    #[test]
    fn site_packages_requires_runtime_cwasm() {
        let python = PythonRuntime {
            site_packages: Some(PathBuf::from("/site")),
            ..PythonRuntime::default()
        };
        assert!(python.builder().is_err());
    }

    /// `--shell-node` registers the embedded JavaScript as `node` and `js`.
    #[cfg(all(feature = "shell", feature = "javascript"))]
    #[tokio::test]
    async fn shell_node_runs_javascript_in_the_shell() {
        let shell = ShellCommands {
            commands: vec![(vec!["node".into(), "js".into()], node_command().unwrap())],
            embedded: true,
        };
        let pool = create_language_pool(
            eryx::Language::Shell,
            PoolConfig::default().with_max_size(1),
            shell,
            PythonRuntime::default(),
            None,
        )
        .await
        .unwrap();
        let sandbox = pool.acquire().await.unwrap();
        let out = sandbox
            .execute("seq 3 | node -e 'console.log(require(\"fs\").readFileSync(0).length)'")
            .await
            .unwrap();
        assert_eq!(out.stdout_utf8().unwrap(), "6\n");
    }

    /// `--shell-command-tunnels` splits a multicall entry by name, and must
    /// name a `--shell-command`.
    #[test]
    fn shell_command_tunnels_name_registered_commands() {
        let coreutils = ShellCommand {
            names: vec!["cat".into(), "head".into()],
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eryx-runtime/coreutils.cwasm"),
            sha256: None,
        };
        let loaded =
            load_shell_commands(std::slice::from_ref(&coreutils), &["head".into()], &[]).unwrap();
        let names: Vec<_> = loaded.iter().map(|(names, _)| names.clone()).collect();
        assert_eq!(names, [vec!["head".to_string()], vec!["cat".to_string()]]);

        let spawning = load_shell_commands(
            std::slice::from_ref(&coreutils),
            &["head".into()],
            &["head".into(), "cat".into()],
        )
        .unwrap();
        let names: Vec<_> = spawning.iter().map(|(names, _)| names.clone()).collect();
        assert_eq!(names, [vec!["head".to_string()], vec!["cat".to_string()]]);
        let err = load_shell_commands(std::slice::from_ref(&coreutils), &[], &["curl".into()])
            .unwrap_err();
        assert_eq!(
            err,
            "--shell-command-spawn curl: not a --shell-command name"
        );

        let err = load_shell_commands(&[coreutils], &["curl".into()], &[]).unwrap_err();
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
                sha256: None,
            }],
            &[],
            &[],
        )
        .unwrap_err();
        assert!(
            err.starts_with("--shell-command rg=/nonexistent/rg.wasm: "),
            "{err}"
        );
    }

    /// An empty ERYX_SHELL_COMMANDS, and empty `;` entries, mean no commands
    /// rather than an error.
    #[test]
    fn empty_shell_commands_are_skipped() {
        let parse = |value: &str| {
            let args = Args::try_parse_from(["eryx-server", "--shell-command", value]).unwrap();
            parse_shell_commands(&args.shell_commands)
        };
        assert!(parse("").unwrap().is_empty());
        assert_eq!(parse(";rg=/opt/rg.wasm;; ").unwrap().len(), 1);
        assert!(parse("rg").is_err());
    }

    /// A pinned `.wasm` must have its digest, and only a `.wasm` can be
    /// pinned; both fail startup naming the flag.
    #[test]
    fn pinned_shell_commands_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let wasm = dir.path().join("rg.wasm");
        std::fs::write(&wasm, b"not the pinned file").unwrap();
        let pin = format!("sha256:{}", "0".repeat(64));
        let pinned = |path: &std::path::Path| {
            load_shell_commands(
                &[ShellCommand {
                    names: vec!["rg".into()],
                    path: path.to_owned(),
                    sha256: Some(pin.clone()),
                }],
                &[],
                &[],
            )
            .map(|_| ())
            .unwrap_err()
        };
        let err = pinned(&wasm);
        assert!(
            err.starts_with(&format!("--shell-command rg={}@{pin}: ", wasm.display())),
            "{err}"
        );
        assert!(err.contains(&pin), "{err}");
        let err = pinned(&dir.path().join("rg.cwasm"));
        assert!(err.contains("only a .wasm can be pinned"), "{err}");
    }

    /// An unpinned `.wasm` is compiled into eryx's command cache, as a
    /// pinned one is, so a restart doesn't compile it again (#1064).
    #[test]
    fn unpinned_shell_commands_use_the_compile_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noop.wasm");
        let mut wasm = wat::parse_str(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func (param i32)))
                (memory (export "memory") 1)
                (func (export "_start")))"#,
        )
        .unwrap();
        // A custom section naming this run, so the cache entry is this
        // test's own and not one an earlier run left behind.
        let run = format!("{:?}", std::time::SystemTime::now());
        wasm.extend([0, u8::try_from(run.len() + 2).unwrap(), 1, b'x']);
        wasm.extend(run.as_bytes());
        std::fs::write(&path, &wasm).unwrap();
        let prefix = format!(
            "sha256-{}-",
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&wasm))
        );

        load_shell_commands(
            &[ShellCommand {
                names: vec!["noop".into()],
                path,
                sha256: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        let commands = eryx::cache::default_cache_root().unwrap().join("commands");
        let cached: Vec<_> = std::fs::read_dir(&commands)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        assert_eq!(cached.len(), 1, "no {prefix}* in {}", commands.display());
        for path in cached {
            std::fs::remove_file(path).unwrap();
        }
    }

    /// A one-module environment named `name` for `language`, in `dir`.
    fn write_environment(dir: &std::path::Path, name: &str, language: eryx::environment::Language) {
        use eryx::environment::{Builder, Inputs, Manifest};
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(dir.join("modules/m.js"), "export const x = 1;\n").unwrap();
        let exports = [("m".to_string(), "modules/m.js".to_string())].into();
        Manifest::new(
            name,
            language,
            Inputs::new(Vec::new()),
            Builder::new("test"),
            dir,
            "modules",
            exports,
        )
        .unwrap()
        .write(dir)
        .unwrap();
    }

    #[test]
    fn environments_load_by_name_and_are_checked() {
        use eryx::environment::Language;
        let all = [eryx::Language::Python, eryx::Language::JavaScript];

        let dir = tempfile::tempdir().unwrap();
        write_environment(&dir.path().join("a"), "tools", Language::JavaScript);
        // Files beside the environments are skipped.
        std::fs::write(dir.path().join("README"), "").unwrap();
        let environments = load_environments(dir.path(), &all).unwrap();
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].manifest().name, "tools");

        // A language the server doesn't serve.
        let err = load_environments(dir.path(), &[eryx::Language::Python]).unwrap_err();
        assert!(err.to_string().contains("doesn't serve"), "{err}");

        // A symlink to one is followed, and dot-entries are skipped.
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            write_environment(elsewhere.path(), "linked", Language::JavaScript);
            std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("linked")).unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("..data")).unwrap();
            let environments = load_environments(dir.path(), &all).unwrap();
            let names: Vec<_> = environments
                .iter()
                .map(|e| e.manifest().name.as_str())
                .collect();
            assert_eq!(names, ["tools", "linked"]);
            std::fs::remove_file(dir.path().join("linked")).unwrap();
        }

        // One without a name.
        let unnamed = tempfile::tempdir().unwrap();
        write_environment(&unnamed.path().join("x"), "", Language::JavaScript);
        let err = load_environments(unnamed.path(), &all).unwrap_err();
        assert!(err.to_string().contains("has no name"), "{err}");

        // Two with one name.
        write_environment(&dir.path().join("b"), "tools", Language::JavaScript);
        let err = load_environments(dir.path(), &all).unwrap_err();
        assert!(err.to_string().contains("both named \"tools\""), "{err}");

        // One that doesn't load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let err = load_environments(dir.path(), &all).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }
}
