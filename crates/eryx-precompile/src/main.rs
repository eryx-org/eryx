//! CLI tool for pre-compiling eryx WASM runtimes.
//!
//! Pre-initializes Python and/or pre-compiles WASM to native code, producing
//! artifacts that dramatically speed up sandbox creation.
//!
//! # Examples
//!
//! ```bash
//! # One-time setup for crates.io users (downloads + compiles for your platform)
//! eryx-precompile setup
//!
//! # Pre-init + compile for Fly.io (x86-64-v3, no AVX-512)
//! eryx-precompile compile runtime.wasm -o runtime.cwasm --preinit --stdlib ./python-stdlib \
//!   --cpu-features x86-64-v3
//!
//! # Pre-init only (output pre-initialized .wasm for later compilation)
//! eryx-precompile compile runtime.wasm -o runtime-preinit.wasm --preinit --stdlib ./python-stdlib --wasm-only
//!
//! # AOT compile only (no pre-init, for already pre-initialized .wasm)
//! eryx-precompile compile runtime-preinit.wasm -o runtime.cwasm --cpu-features x86-64-v3
//!
//! # Cross-compile for aarch64
//! eryx-precompile compile runtime.wasm -o runtime.cwasm --target aarch64-unknown-linux-gnu
//!
//! # Pre-init with packages (wheels, tar.gz, or directories)
//! eryx-precompile compile runtime.wasm -o runtime-numpy.cwasm --preinit --stdlib ./python-stdlib \
//!   --package numpy-2.2.3-wasi.tar.gz --import numpy
//!
//! # Pre-init with site-packages directory
//! eryx-precompile compile runtime.wasm -o runtime.cwasm --preinit --stdlib ./python-stdlib \
//!   --site-packages ./my-site-packages --import jinja2
//!
//! # Verify packages work (not just import)
//! eryx-precompile compile runtime.wasm -o numpy.cwasm --preinit --stdlib ./python-stdlib \
//!   --package numpy-wasi.tar.gz --import numpy \
//!   --verify-code "import numpy; print(numpy.array(\[1,2,3\]).sum())"
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use eryx::{CpuFeatureLevel, Session}; // For execute() method and CPU features
use sha2::{Digest, Sha256};

/// Largest `runtime.wasm` that `setup` will download (the real one is ~50 MB).
const MAX_RUNTIME_WASM_BYTES: u64 = 256 * 1024 * 1024;

/// Largest `runtime.wasm.sha256` that `setup` will download.
const MAX_CHECKSUM_BYTES: u64 = 1024;

/// Pre-compile eryx WASM runtimes for fast sandbox creation.
#[derive(Parser, Debug)]
#[command(name = "eryx-precompile")]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download and pre-compile the WASM runtime for your platform.
    ///
    /// Downloads runtime.wasm from the matching GitHub Release, pre-compiles it
    /// to native code, and caches it in ~/.cache/eryx/ so that `cargo build`
    /// with `features = ["embedded"]` finds it automatically.
    ///
    /// For cross-compilation, custom CPU targets, or pre-initialization with
    /// packages, use the `compile` subcommand instead.
    Setup(SetupArgs),

    /// Pre-compile a WASM file to native code (advanced).
    ///
    /// For direct control over pre-initialization and AOT compilation.
    Compile(Box<CompileArgs>),
}

#[derive(Parser, Debug)]
struct SetupArgs {
    /// Version to download (default: version of this binary)
    ///
    /// Must match an existing GitHub Release with a runtime.wasm asset.
    #[arg(long)]
    version: Option<String>,

    /// Expected SHA-256 of runtime.wasm, as hex
    ///
    /// By default the checksum comes from the `runtime.wasm.sha256` asset of
    /// the same release. Releases before 0.12.0 don't publish one, so pass the
    /// hash here to set them up.
    #[arg(long, value_name = "HEX")]
    sha256: Option<String>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Parser, Debug)]
struct CompileArgs {
    /// Input WASM file (.wasm or pre-initialized .wasm)
    #[arg(required = true)]
    input: PathBuf,

    /// Output path (default: input with .cwasm extension, or .wasm for --wasm-only)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Cross-compilation target triple (e.g. aarch64-unknown-linux-gnu)
    ///
    /// When omitted, compiles for the host architecture. For controlling
    /// CPU feature levels (SSE, AVX, etc.), use --cpu-features instead.
    #[arg(short, long)]
    target: Option<String>,

    /// CPU feature level for AOT compilation
    ///
    /// Controls which instruction set level the artifact targets.
    /// Lower levels are more portable; higher levels produce faster code.
    ///
    /// Values:
    ///   native     - Host CPU features (fastest but not portable)
    ///   x86-64-v4  - AVX-512 (~2017+ Skylake-X)
    ///   x86-64-v3  - AVX2, FMA, BMI1/2 (~2013+ Haswell, recommended for Fly.io)
    ///   x86-64-v2  - SSE4.2, POPCNT (~2008+ Nehalem)
    ///   x86-64     - Baseline SSE2 (maximum compatibility)
    ///   baseline   - Target architecture baseline, no optional features
    ///
    /// Defaults to native when omitted, with a warning, since the result only
    /// runs on CPUs with the same features as this one.
    #[arg(short = 'c', long, value_name = "LEVEL")]
    cpu_features: Option<CpuFeatureLevel>,

    /// Pre-initialize Python before compiling
    ///
    /// This runs Python's interpreter initialization and captures the memory state,
    /// reducing session creation time from ~450ms to ~1-5ms.
    #[arg(long)]
    preinit: bool,

    /// Path to Python stdlib (required with --preinit)
    #[arg(long, required_if_eq("preinit", "true"))]
    stdlib: Option<PathBuf>,

    /// Path to a site-packages directory
    ///
    /// Mount a directory containing Python packages. Any .so files found
    /// will be linked as native extensions. Can be combined with --package,
    /// in which case both are merged into a temporary directory and this one
    /// is left untouched. Requires --preinit.
    #[arg(long, requires = "preinit")]
    site_packages: Option<PathBuf>,

    /// Package file to include (.whl, .tar.gz, or directory)
    ///
    /// Packages are extracted and their contents are made available as
    /// site-packages. Native extensions (.so files) are automatically
    /// linked. Can be specified multiple times. Requires --preinit.
    #[arg(long = "package", value_name = "PATH", requires = "preinit")]
    packages: Vec<PathBuf>,

    /// Module to pre-import during initialization
    ///
    /// Pre-importing modules captures their initialized state in the snapshot,
    /// making them instantly available at runtime. Can be specified multiple times.
    ///
    /// Requires --preinit.
    ///
    /// Example: --import numpy --import pandas
    #[arg(long = "import", value_name = "MODULE", requires = "preinit")]
    imports: Vec<String>,

    /// Output pre-initialized .wasm instead of native .cwasm
    ///
    /// Use this to create architecture-independent pre-initialized artifacts
    /// that can be AOT compiled later for different targets.
    #[arg(long, conflicts_with_all = ["target", "cpu_features"])]
    wasm_only: bool,

    /// Skip verification step
    #[arg(long)]
    no_verify: bool,

    /// Python code to execute during pre-initialization (baked into snapshot)
    ///
    /// Runs after imports, before the memory snapshot is taken. Use this to
    /// pre-create objects that every sandbox should start with (e.g., a Jinja2
    /// SandboxedEnvironment with filters registered). The state is captured in
    /// COW memory, so each sandbox gets its own isolated copy.
    ///
    /// Mutually exclusive with --setup-file. Requires --preinit.
    ///
    /// Example: `--setup-code "from jinja2.sandbox import SandboxedEnvironment; env = SandboxedEnvironment()"`
    #[arg(
        long,
        value_name = "CODE",
        conflicts_with = "setup_file",
        requires = "preinit"
    )]
    setup_code: Option<String>,

    /// Read setup code from a file instead of inline
    ///
    /// Like --setup-code, but reads the Python code from a file. Useful for
    /// longer setup scripts. Requires --preinit.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "setup_code",
        requires = "preinit"
    )]
    setup_file: Option<PathBuf>,

    /// JSON file declaring the callbacks sandboxes will register (baked into snapshot)
    ///
    /// A list of objects with "name", "description" and optional "parameters"
    /// (a JSON Schema object, `{}` if omitted). Their Python wrappers are
    /// installed during pre-initialization, so a fresh instance whose host
    /// registers callbacks with the same names, descriptions and schemas skips
    /// the per-execution callback setup. Requires --preinit.
    ///
    /// Example: `--callbacks callbacks.json` with
    /// `[{"name": "get_time", "description": "Current time", "parameters": {"type": "object"}}]`
    #[arg(long, value_name = "PATH", requires = "preinit")]
    callbacks: Option<PathBuf>,

    /// Python code to execute during verification
    ///
    /// Runs after the standard import verification. Useful for testing that
    /// packages actually work, not just that they import.
    ///
    /// Example: `--verify-code "import numpy; print(numpy.array(\[1,2,3\]).sum())"`
    #[arg(long, value_name = "CODE")]
    verify_code: Option<String>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install the TLS crypto provider before any reqwest/rustls usage.
    // We use aws-lc-rs to match the workspace and avoid conflicts from
    // feature unification (see commit 1897e8a).
    if let Err(_already_set) = rustls::crypto::aws_lc_rs::default_provider().install_default() {
        // Another provider was already installed; that's fine.
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Setup(args) => run_setup(args).await,
        Command::Compile(args) => run_compile(*args).await,
    }
}

/// Determine the cache directory for eryx runtime artifacts.
fn cache_dir() -> Result<PathBuf> {
    let dir = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("eryx")
    } else {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .context("Could not determine home directory (set HOME or XDG_CACHE_HOME)")?;
        PathBuf::from(home).join(".cache").join("eryx")
    };
    Ok(dir)
}

/// Build a short platform target string like "x86_64-linux" or "aarch64-macos".
fn platform_target() -> String {
    let arch = std::env::consts::ARCH; // "x86_64", "aarch64", etc.
    let os = std::env::consts::OS; // "linux", "macos", "windows"
    format!("{arch}-{os}")
}

/// Try to download runtime.wasm from a GitHub Release and verify its SHA-256.
///
/// Tries multiple tag patterns since the naming convention has changed over time:
/// - `v{version}` (cargo-dist releases, used for v0.3.0 and earlier)
/// - `eryx-v{version}` (release-plz releases, used from v0.4.0+)
/// - `eryx-precompile-v{version}` (upload-runtime attaches to whichever release triggers it)
///
/// The expected hash is `expected_sha256` if given, otherwise the
/// `runtime.wasm.sha256` asset next to `runtime.wasm` in the same release.
async fn download_runtime_wasm(
    client: &reqwest::Client,
    version: &str,
    expected_sha256: Option<&str>,
) -> Result<Vec<u8>> {
    let tag_patterns = [
        format!("eryx-v{version}"),
        format!("eryx-precompile-v{version}"),
        format!("v{version}"),
    ];

    let mut last_error = None;
    for tag in &tag_patterns {
        let url = format!("https://github.com/eryx-org/eryx/releases/download/{tag}/runtime.wasm");
        println!("  Trying {url}");

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                println!("  Found runtime.wasm in release {tag}");
                let expected = match expected_sha256 {
                    Some(hex) => parse_sha256(hex)?,
                    None => fetch_sha256(client, &format!("{url}.sha256")).await?,
                };
                let bytes = read_capped(resp, MAX_RUNTIME_WASM_BYTES)
                    .await
                    .context("Failed to download runtime.wasm body")?;
                verify_sha256(&bytes, &expected)?;
                println!("  Verified SHA-256 {}", hex::encode(expected));
                return Ok(bytes);
            }
            Ok(resp) => {
                last_error = Some(anyhow::anyhow!("GET {url} returned HTTP {}", resp.status()));
            }
            Err(e) => {
                last_error = Some(e.into());
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No tag patterns to try"))).context(format!(
        "Could not find runtime.wasm in any GitHub Release for version {version}.\n\
             Tried tags: {}.\n\
             Check https://github.com/eryx-org/eryx/releases for available versions.",
        tag_patterns.join(", ")
    ))
}

/// Download the checksum published at `url`.
async fn fetch_sha256(client: &reqwest::Client, url: &str) -> Result<[u8; 32]> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "GET {url} returned HTTP {}. Releases before 0.12.0 don't publish a checksum; \
             pass the expected hash with --sha256",
            resp.status()
        );
    }
    let body = read_capped(resp, MAX_CHECKSUM_BYTES)
        .await
        .with_context(|| format!("Failed to download {url}"))?;
    parse_sha256(&String::from_utf8_lossy(&body))
        .with_context(|| format!("Invalid checksum in {url}"))
}

/// Read a response body, failing once it grows past `max` bytes.
async fn read_capped(mut resp: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len > max
    {
        anyhow::bail!("response is {len} bytes, more than the {max} byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if (body.len() + chunk.len()) as u64 > max {
            anyhow::bail!("response is more than the {max} byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parse a SHA-256 from hex, or from `sha256sum` output (`<hex>  <file>`).
fn parse_sha256(text: &str) -> Result<[u8; 32]> {
    let hex = text.split_whitespace().next().unwrap_or_default();
    let mut hash = [0u8; 32];
    hex::decode_to_slice(hex, &mut hash)
        .with_context(|| format!("expected a 64-character hex SHA-256, got {hex:?}"))?;
    Ok(hash)
}

fn verify_sha256(bytes: &[u8], expected: &[u8; 32]) -> Result<()> {
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    if actual != *expected {
        anyhow::bail!(
            "SHA-256 mismatch: expected {}, got {}",
            hex::encode(expected),
            hex::encode(actual)
        );
    }
    Ok(())
}

/// Path of the checksum file kept next to a cached runtime.
fn checksum_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

/// Whether `path` holds a complete runtime written by `setup`, according to
/// the checksum file stored next to it.
fn cached_runtime_is_valid(path: &Path) -> bool {
    let (Ok(bytes), Ok(checksum)) = (
        std::fs::read(path),
        std::fs::read_to_string(checksum_path(path)),
    ) else {
        return false;
    };
    parse_sha256(&checksum).is_ok_and(|expected| verify_sha256(&bytes, &expected).is_ok())
}

/// Write `bytes` to `path` through a temp file in the same directory, so
/// readers see either the old file or the complete new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Cache `cwasm` at `path`, with its checksum next to it. The checksum is
/// written last, so an interrupted run leaves an entry that fails validation.
fn write_cached_runtime(path: &Path, cwasm: &[u8]) -> Result<()> {
    let checksum = checksum_path(path);
    // Drop any stale checksum first, so it can never vouch for the new file
    // if the checksum write below doesn't happen.
    if let Err(e) = std::fs::remove_file(&checksum)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e).with_context(|| format!("Failed to remove {}", checksum.display()));
    }
    write_atomic(path, cwasm)?;
    let hash = hex::encode(Sha256::digest(cwasm));
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    write_atomic(&checksum, format!("{hash}  {file_name}\n").as_bytes())
}

async fn run_setup(args: SetupArgs) -> Result<()> {
    let filter = if args.verbose {
        "eryx=debug,eryx_precompile=debug"
    } else {
        "eryx=warn,eryx_precompile=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let version = args
        .version
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let target = platform_target();
    let cache = cache_dir()?;
    let cwasm_name = format!("runtime-v{version}-{target}.cwasm");
    let cwasm_path = cache.join(&cwasm_name);

    println!("eryx-precompile setup");
    println!("=====================");
    println!();
    println!("Version:  {version}");
    println!("Platform: {target}");
    println!("Cache:    {}", cache.display());
    println!();

    // Check if already cached. A file without a matching checksum (from an
    // interrupted run or an older version of this tool) is rebuilt.
    if cached_runtime_is_valid(&cwasm_path) {
        println!("Already cached: {}", cwasm_path.display());
        println!();
        println!(
            "To force re-download, delete the file and run again:\n  rm {}",
            cwasm_path.display()
        );
        return Ok(());
    }

    // Step 1: Download runtime.wasm
    println!("Step 1: Downloading runtime.wasm from GitHub Release...");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(format!("eryx-precompile/{version}"))
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("Failed to create HTTP client")?;

    let wasm_bytes = download_runtime_wasm(&client, &version, args.sha256.as_deref()).await?;

    println!(
        "  Downloaded {} bytes ({:.1} MB)",
        wasm_bytes.len(),
        wasm_bytes.len() as f64 / 1_000_000.0
    );

    // Step 2: Pre-compile to native code
    println!();
    println!("Step 2: Pre-compiling to native code (this may take a minute)...");
    let start = Instant::now();

    // The cache file is named for the host platform only, so compile for
    // exactly that: `precompile` would also honour ERYX_TARGET and friends,
    // and could cache an artifact under a name that does not describe it.
    let precompiled =
        eryx::PythonExecutor::precompile_with_options(&wasm_bytes, None, CpuFeatureLevel::Native)
            .context("Failed to pre-compile runtime.wasm to native code")?;

    let elapsed = start.elapsed();
    println!(
        "  Done in {elapsed:?} ({} bytes, {:.1} MB)",
        precompiled.len(),
        precompiled.len() as f64 / 1_000_000.0
    );

    // Step 3: Write to cache
    println!();
    println!("Step 3: Writing to cache...");
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("Failed to create cache directory: {}", cache.display()))?;
    write_cached_runtime(&cwasm_path, &precompiled)?;
    println!("  Saved to: {}", cwasm_path.display());

    println!();
    println!(
        "Setup complete! `cargo build` with `features = [\"embedded\"]` will now find the cached runtime."
    );
    println!();
    println!(
        "Tip: for cross-compilation or custom CPU targets, use `eryx-precompile compile` instead."
    );

    Ok(())
}

/// One entry of the `--callbacks` file.
#[derive(serde::Deserialize)]
struct CallbackDeclarationFile {
    name: String,
    #[serde(default)]
    description: String,
    /// JSON Schema for the arguments; an empty object when omitted.
    #[serde(default = "empty_object")]
    parameters: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Read the callback declarations to bake into the snapshot from `path`.
fn read_callback_declarations(path: &Path) -> Result<Vec<eryx::preinit::CallbackDeclaration>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read callbacks file: {}", path.display()))?;
    let entries: Vec<CallbackDeclarationFile> = serde_json::from_str(&text)
        .with_context(|| format!("Invalid callbacks file: {}", path.display()))?;
    entries
        .into_iter()
        .map(|entry| {
            Ok(eryx::preinit::CallbackDeclaration {
                name: entry.name,
                description: entry.description,
                parameters_schema_json: serde_json::to_string(&entry.parameters)
                    .context("Failed to serialize callback parameters schema")?,
            })
        })
        .collect()
}

async fn run_compile(args: CompileArgs) -> Result<()> {
    // Set up tracing
    let filter = if args.verbose {
        "eryx=debug,eryx_precompile=debug"
    } else {
        "eryx=warn,eryx_precompile=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Determine output path
    let output = args.output.unwrap_or_else(|| {
        let mut out = args.input.clone();
        if args.wasm_only {
            out.set_extension("preinit.wasm");
        } else {
            out.set_extension("cwasm");
        }
        out
    });

    // Resolve target triple and CPU feature level from the flags.
    let (target_triple, cpu_features) =
        resolve_target_and_features(args.target.as_deref(), args.cpu_features)?;
    if !args.wasm_only
        && target_triple.is_none()
        && args.cpu_features.is_none()
        && cpu_features == CpuFeatureLevel::Native
    {
        eprintln!(
            "warning: no --cpu-features given, compiling for this machine's CPU (native). \
             The artifact may not load on other machines; pass --cpu-features \
             (e.g. baseline or x86-64-v3) for a portable build."
        );
    }

    println!("eryx-precompile");
    println!("===============");
    println!();
    println!("Input:   {}", args.input.display());
    println!("Output:  {}", output.display());
    if !args.wasm_only {
        if let Some(ref triple) = target_triple {
            println!("Target:  {triple}");
        }
        println!("CPU:     {cpu_features}");
    }
    if args.preinit {
        println!(
            "Stdlib:  {}",
            args.stdlib
                .as_ref()
                .map_or("-", |p| p.to_str().unwrap_or("-"))
        );
    }
    if let Some(ref site_pkg) = args.site_packages {
        println!("Site-packages: {}", site_pkg.display());
    }
    if !args.packages.is_empty() {
        println!(
            "Packages: {}",
            args.packages
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !args.imports.is_empty() {
        println!("Imports: {}", args.imports.join(", "));
    }

    // Resolve setup code from --setup-code or --setup-file
    let setup_code = if let Some(code) = args.setup_code {
        Some(code)
    } else if let Some(ref path) = args.setup_file {
        let code = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read setup file: {}", path.display()))?;
        Some(code)
    } else {
        None
    };

    if setup_code.is_some() {
        println!(
            "Setup:   {} (baked into snapshot)",
            args.setup_file
                .as_ref()
                .map_or("inline code".to_string(), |p| p.display().to_string())
        );
    }

    let callbacks = match &args.callbacks {
        Some(path) => read_callback_declarations(path)?,
        None => Vec::new(),
    };
    if !callbacks.is_empty() {
        println!(
            "Callbacks: {} (baked into snapshot)",
            callbacks
                .iter()
                .map(|cb| cb.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();

    // Read input WASM
    let wasm_bytes = std::fs::read(&args.input)
        .with_context(|| format!("Failed to read input file: {}", args.input.display()))?;
    println!(
        "Input size: {} bytes ({:.1} MB)",
        wasm_bytes.len(),
        wasm_bytes.len() as f64 / 1_000_000.0
    );

    // Step 1: Pre-initialize Python if requested
    let component_bytes = if args.preinit {
        let stdlib = args
            .stdlib
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--stdlib required with --preinit"))?;

        // Validate stdlib exists
        if !stdlib.exists() || !stdlib.join("encodings").exists() {
            anyhow::bail!(
                "Invalid Python stdlib at {}: expected 'encodings' subdirectory",
                stdlib.display()
            );
        }

        // Process packages and site-packages
        let mut packages = process_packages(args.site_packages.as_ref(), &args.packages)?;
        let extensions = std::mem::take(&mut packages.extensions);

        if let Some(ref sp) = packages.site_packages {
            println!("Site-packages dir: {}", sp.display());
        }
        if !extensions.is_empty() {
            println!("Native extensions: {}", extensions.len());
        }

        println!();
        println!("Step 1: Pre-initializing Python...");
        let start = Instant::now();

        let mut options = eryx::preinit::PreInitOptions::new(stdlib)
            .imports(args.imports.iter().cloned())
            .extensions(extensions)
            .callbacks(callbacks);
        if let Some(path) = &packages.site_packages {
            options = options.site_packages(path);
        }
        if let Some(code) = &setup_code {
            options = options.setup_code(code.clone());
        }
        let preinit_bytes = eryx::preinit::pre_initialize_with_options(options)
            .await
            .context("Failed to pre-initialize Python")?;

        let elapsed = start.elapsed();
        println!(
            "  Done in {elapsed:?} ({} bytes, {:.1} MB)",
            preinit_bytes.len(),
            preinit_bytes.len() as f64 / 1_000_000.0
        );

        preinit_bytes
    } else {
        println!();
        println!("Step 1: Skipping pre-initialization (use --preinit to enable)");
        wasm_bytes
    };

    // Step 2: Either output WASM or AOT compile
    if args.wasm_only {
        println!();
        println!("Step 2: Writing pre-initialized WASM...");
        std::fs::write(&output, &component_bytes)
            .with_context(|| format!("Failed to write output: {}", output.display()))?;
        println!(
            "  Saved to: {} ({} bytes)",
            output.display(),
            component_bytes.len()
        );
    } else {
        println!();
        println!("Step 2: AOT compiling to native code...");
        let start = Instant::now();

        let precompiled = eryx::PythonExecutor::precompile_with_options(
            &component_bytes,
            target_triple.as_deref(),
            cpu_features,
        )
        .context("Failed to AOT compile")?;

        let elapsed = start.elapsed();
        println!(
            "  Done in {elapsed:?} ({} bytes, {:.1} MB)",
            precompiled.len(),
            precompiled.len() as f64 / 1_000_000.0
        );

        // Write output
        println!();
        println!("Step 3: Writing output...");
        std::fs::write(&output, &precompiled)
            .with_context(|| format!("Failed to write output: {}", output.display()))?;
        println!("  Saved to: {}", output.display());

        // Verify if requested
        if !args.no_verify {
            println!();
            if let Some(reason) = unrunnable_reason(target_triple.as_deref(), cpu_features) {
                println!("Step 4: Skipping verification: {reason}");
            } else {
                println!("Step 4: Verifying...");
                verify_cwasm(
                    &output,
                    args.stdlib.as_deref(),
                    &args.imports,
                    args.verify_code.as_deref(),
                )
                .await?;
                println!("  Verification passed!");
            }
        }
    }

    println!();
    println!("Success!");
    Ok(())
}

/// The result of [`process_packages`].
struct ProcessedPackages {
    /// Directory to mount at `/site-packages`, if any.
    site_packages: Option<PathBuf>,
    extensions: Vec<eryx::preinit::NativeExtension>,
    /// Held so the temp directories `site_packages` may point into outlive
    /// pre-initialization.
    _extracted_packages: Vec<eryx::ExtractedPackage>,
    _merged_dir: Option<tempfile::TempDir>,
}

/// Process packages and site-packages to extract native extensions.
///
/// With a single source (one `--site-packages` or one `--package`) its
/// directory is used as is. With several, they are merged into a fresh temp
/// directory, so no source directory is ever written to.
fn process_packages(
    site_packages: Option<&PathBuf>,
    packages: &[PathBuf],
) -> Result<ProcessedPackages> {
    let mut extensions = Vec::new();
    let mut extracted_packages = Vec::new();

    let merged_dir = if usize::from(site_packages.is_some()) + packages.len() > 1 {
        let dir = tempfile::tempdir().context("Failed to create merged site-packages directory")?;
        if let Some(site_packages) = site_packages {
            copy_directory_contents(site_packages, dir.path()).with_context(|| {
                format!("Failed to copy site-packages: {}", site_packages.display())
            })?;
        }
        Some(dir)
    } else {
        None
    };
    let mut final_site_packages = merged_dir
        .as_ref()
        .map(|dir| dir.path().to_path_buf())
        .or_else(|| site_packages.cloned());

    // Extract each package and collect native extensions
    for path in packages {
        println!("Extracting package: {}", path.display());
        let package = eryx::ExtractedPackage::from_path(path)
            .with_context(|| format!("Failed to extract package: {}", path.display()))?;

        println!(
            "  {} (native extensions: {})",
            package.name,
            package.native_extensions.len()
        );

        match &merged_dir {
            Some(dir) => copy_directory_contents(&package.python_path, dir.path())
                .with_context(|| format!("Failed to merge package: {}", package.name))?,
            None => final_site_packages = Some(package.python_path.clone()),
        }

        // Collect native extensions with dlopen paths relative to /site-packages
        for ext in &package.native_extensions {
            let dlopen_path = format!("/site-packages/{}", ext.relative_path);
            extensions.push(eryx::preinit::NativeExtension::new(
                dlopen_path,
                ext.bytes.clone(),
            ));
        }

        extracted_packages.push(package);
    }

    // Scan site-packages directory for additional native extensions
    if let Some(ref site_pkg_path) = final_site_packages
        && site_pkg_path.exists()
    {
        for entry in walkdir::WalkDir::new(site_pkg_path) {
            let entry = entry.context("Failed to walk site-packages directory")?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "so") {
                let relative = path
                    .strip_prefix(site_pkg_path)
                    .context("Failed to compute relative path")?;
                let relative_str = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                let dlopen_path = format!("/site-packages/{relative_str}");

                // Skip if we already have this extension from packages
                if extensions.iter().any(|e| e.name == dlopen_path) {
                    continue;
                }

                let bytes = std::fs::read(path).context("Failed to read native extension .so")?;
                extensions.push(eryx::preinit::NativeExtension::new(dlopen_path, bytes));
            }
        }
    }

    Ok(ProcessedPackages {
        site_packages: final_site_packages,
        extensions,
        _extracted_packages: extracted_packages,
        _merged_dir: merged_dir,
    })
}

/// Copy contents of one directory into another.
fn copy_directory_contents(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.context("Failed to walk directory")?;
        let src_path = entry.path();
        let relative = src_path
            .strip_prefix(src)
            .context("Failed to compute relative path")?;
        let dst_path = dst.join(relative);

        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path).context("Failed to create directory")?;
        } else if src_path.is_file() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent).context("Failed to create parent directory")?;
            }
            std::fs::copy(src_path, &dst_path).context("Failed to copy file")?;
        }
    }
    Ok(())
}

/// Resolve the `--target` and `--cpu-features` flags into a target triple and
/// CPU feature level.
///
/// Handles backward compatibility: `--target x86-64-v3` (without `--cpu-features`)
/// is treated as `--cpu-features x86-64-v3`.
fn resolve_target_and_features(
    target: Option<&str>,
    cpu_features: Option<CpuFeatureLevel>,
) -> Result<(Option<String>, CpuFeatureLevel)> {
    match (target, cpu_features) {
        (None, None) => Ok((None, CpuFeatureLevel::Native)),
        (None, Some(level)) => Ok((None, level)),
        (Some(t), None) => {
            // Backward compat: --target x86-64-v3 still works.
            if let Some(level) = CpuFeatureLevel::parse(t) {
                eprintln!(
                    "note: use --cpu-features {t} instead of --target {t} \
                     (--target is now for cross-compilation triples)"
                );
                Ok((None, level))
            } else {
                Ok((Some(t.to_string()), CpuFeatureLevel::Native))
            }
        }
        (Some(t), Some(_)) if CpuFeatureLevel::parse(t).is_some() => {
            anyhow::bail!(
                "--target {t} looks like a CPU feature level, not a target triple. \
                 Use --cpu-features {t} instead, or pass a real triple to --target."
            );
        }
        (Some(t), Some(level)) => Ok((Some(t.to_string()), level)),
    }
}

/// Why an artifact compiled for `target` and `cpu_features` cannot be loaded
/// on this machine, or `None` if it can.
fn unrunnable_reason(target: Option<&str>, cpu_features: CpuFeatureLevel) -> Option<String> {
    if let Some(target) = target {
        let host = env!("ERYX_PRECOMPILE_HOST_TARGET");
        if target != host {
            return Some(format!(
                "artifact targets {target}, which cannot run on this host ({host})"
            ));
        }
    }
    let missing = missing_host_features(cpu_features);
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "this host lacks {} required by --cpu-features {cpu_features}",
            missing.join(", ")
        ))
    }
}

/// The features of the x86-64 psABI `level` this CPU does not have.
#[cfg(target_arch = "x86_64")]
fn missing_host_features(level: CpuFeatureLevel) -> Vec<&'static str> {
    macro_rules! missing {
        ($($feature:tt),*) => {
            [$((std::arch::is_x86_feature_detected!($feature), $feature)),*]
                .into_iter()
                .filter_map(|(present, name)| (!present).then_some(name))
                .collect()
        };
    }
    let mut missing: Vec<&'static str> = Vec::new();
    // Each level includes the ones below it.
    if matches!(
        level,
        CpuFeatureLevel::X86_64v2 | CpuFeatureLevel::X86_64v3 | CpuFeatureLevel::X86_64v4
    ) {
        missing.extend::<Vec<_>>(missing!(
            "cmpxchg16b",
            "popcnt",
            "sse3",
            "sse4.1",
            "sse4.2",
            "ssse3"
        ));
    }
    if matches!(level, CpuFeatureLevel::X86_64v3 | CpuFeatureLevel::X86_64v4) {
        missing.extend::<Vec<_>>(missing!(
            "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe"
        ));
    }
    if level == CpuFeatureLevel::X86_64v4 {
        missing.extend::<Vec<_>>(missing!(
            "avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"
        ));
    }
    missing
}

/// psABI levels are x86-64 only, and compiling one for a non-x86-64 host is
/// rejected before verification is reached.
#[cfg(not(target_arch = "x86_64"))]
fn missing_host_features(_level: CpuFeatureLevel) -> Vec<&'static str> {
    Vec::new()
}

/// Verify that the compiled cwasm file works correctly.
async fn verify_cwasm(
    cwasm_path: &PathBuf,
    stdlib: Option<&std::path::Path>,
    imports: &[String],
    verify_code: Option<&str>,
) -> Result<()> {
    let stdlib_path = if let Some(path) = stdlib {
        path.to_path_buf()
    } else {
        // Try to find stdlib in common locations
        let candidates = [
            "crates/eryx-wasm-runtime/tests/python-stdlib",
            "../eryx-wasm-runtime/tests/python-stdlib",
        ];
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists() && p.join("encodings").exists())
            .ok_or_else(|| {
                anyhow::anyhow!("Cannot verify: stdlib not found. Use --no-verify or --stdlib")
            })?
    };

    // Create a sandbox using the compiled cwasm
    // SAFETY: We just created this file from PythonExecutor::precompile()
    #[allow(unsafe_code)]
    let sandbox = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_file(cwasm_path)
            .with_python_stdlib(&stdlib_path)
            .build()
            .context("Failed to create sandbox from compiled cwasm")?
    };

    // Create a session and run a simple test
    let mut session = eryx::InProcessSession::new(&sandbox)
        .await
        .context("Failed to create session")?;

    let result = session
        .execute("print('eryx-precompile verification OK')")
        .await
        .context("Failed to execute test code")?;

    let stdout = result.stdout_text();
    if !stdout.contains("OK") {
        anyhow::bail!("Verification failed: unexpected output: {}", stdout);
    }

    // Verify that pre-imported modules are available
    if !imports.is_empty() {
        let import_code = imports
            .iter()
            .map(|m| format!("import {m}"))
            .collect::<Vec<_>>()
            .join("\n");
        let verify_code = format!("{import_code}\nprint('imports OK')");

        let result = session
            .execute(&verify_code)
            .await
            .context("Failed to verify imports")?;

        let stdout = result.stdout_text();
        if !stdout.contains("imports OK") {
            let error_detail = if result.stderr.is_empty() {
                stdout
            } else {
                result.stderr_text()
            };
            anyhow::bail!(
                "Import verification failed for [{}]: {}",
                imports.join(", "),
                error_detail
            );
        }

        println!("  Imports verified: {}", imports.join(", "));
    }

    // Run custom verification code
    if let Some(code) = verify_code {
        println!("  Running verify code...");

        let result = session
            .execute(code)
            .await
            .context("Failed to execute verify code")?;

        if !result.stderr.is_empty() {
            anyhow::bail!("Verify code produced stderr:\n{}", result.stderr_text());
        }

        if !result.stdout.is_empty() {
            print!("  Output: {}", result.stdout_text());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one HTTP response with a chunked body of `len` bytes, and return its URL.
    async fn serve_chunked_body(len: usize) -> Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await?;
            let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            response.extend_from_slice(format!("{len:x}\r\n").as_bytes());
            response.resize(response.len() + len, b'x');
            response.extend_from_slice(b"\r\n0\r\n\r\n");
            stream.write_all(&response).await?;
            stream.shutdown().await
        });
        Ok(format!("http://{addr}/"))
    }

    #[tokio::test]
    async fn download_size_limit_is_enforced() -> Result<()> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let client = reqwest::Client::new();

        let resp = client.get(serve_chunked_body(100).await?).send().await?;
        assert_eq!(read_capped(resp, 100).await?.len(), 100);

        let resp = client.get(serve_chunked_body(101).await?).send().await?;
        assert!(read_capped(resp, 100).await.is_err());
        Ok(())
    }

    #[test]
    fn checksum_parsing_accepts_sha256sum_output() -> Result<()> {
        let hex = "ab".repeat(32);
        assert_eq!(parse_sha256(&hex)?, [0xab; 32]);
        assert_eq!(parse_sha256(&format!("{hex}  runtime.wasm\n"))?, [0xab; 32]);
        assert!(parse_sha256(&hex[..62]).is_err());
        assert!(parse_sha256("").is_err());
        Ok(())
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let expected: [u8; 32] = Sha256::digest(b"runtime").into();
        assert!(verify_sha256(b"runtime", &expected).is_ok());
        assert!(verify_sha256(b"runtim", &expected).is_err());
    }

    #[test]
    fn cached_runtime_must_match_its_checksum() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("runtime.cwasm");
        assert!(!cached_runtime_is_valid(&path));

        write_cached_runtime(&path, b"compiled runtime")?;
        assert!(cached_runtime_is_valid(&path));

        // A truncated file no longer counts as cached.
        std::fs::write(&path, b"compiled")?;
        assert!(!cached_runtime_is_valid(&path));

        // Nor does a file whose checksum is missing.
        write_cached_runtime(&path, b"compiled runtime")?;
        std::fs::remove_file(checksum_path(&path))?;
        assert!(!cached_runtime_is_valid(&path));
        Ok(())
    }

    #[test]
    fn package_flags_require_preinit() {
        for flag in [
            ["--site-packages", "sp"],
            ["--package", "pkg.whl"],
            ["--import", "numpy"],
        ] {
            let base = ["eryx-precompile", "compile", "runtime.wasm"];
            assert!(Cli::try_parse_from(base.iter().chain(&flag)).is_err());
            assert!(
                Cli::try_parse_from(
                    base.iter()
                        .chain(&["--preinit", "--stdlib", "stdlib"])
                        .chain(&flag)
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn merging_packages_leaves_sources_untouched() -> Result<()> {
        let site_packages = tempfile::tempdir()?;
        std::fs::write(site_packages.path().join("a.py"), "")?;
        let package = tempfile::tempdir()?;
        std::fs::create_dir(package.path().join("b"))?;
        std::fs::write(package.path().join("b").join("__init__.py"), "")?;

        let packages = process_packages(
            Some(&site_packages.path().to_path_buf()),
            &[package.path().to_path_buf()],
        )?;
        let merged = packages
            .site_packages
            .as_ref()
            .context("expected a site-packages dir")?;

        assert_ne!(merged, site_packages.path());
        assert!(merged.join("a.py").is_file());
        assert!(merged.join("b").join("__init__.py").is_file());
        assert!(!site_packages.path().join("b").exists());
        Ok(())
    }

    #[test]
    fn verification_skipped_for_foreign_target() {
        assert!(
            unrunnable_reason(
                Some("riscv64gc-unknown-none-elf"),
                CpuFeatureLevel::Baseline
            )
            .is_some()
        );
        assert!(unrunnable_reason(None, CpuFeatureLevel::Native).is_none());
        assert!(
            unrunnable_reason(
                Some(env!("ERYX_PRECOMPILE_HOST_TARGET")),
                CpuFeatureLevel::Baseline
            )
            .is_none()
        );
    }
}
