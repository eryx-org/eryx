//! CLI tool for pre-compiling eryx WASM runtimes.
//!
//! This tool pre-initializes Python and/or pre-compiles WASM to native code,
//! producing artifacts that dramatically speed up sandbox creation.
//!
//! # Examples
//!
//! ```bash
//! # Pre-init + compile for Fly.io (x86-64-v3, no AVX-512)
//! eryx-precompile runtime.wasm -o runtime.cwasm --preinit --stdlib ./python-stdlib --target x86-64-v3
//!
//! # Pre-init only (output pre-initialized .wasm for later compilation)
//! eryx-precompile runtime.wasm -o runtime-preinit.wasm --preinit --stdlib ./python-stdlib --wasm-only
//!
//! # AOT compile only (no pre-init, for already pre-initialized .wasm)
//! eryx-precompile runtime-preinit.wasm -o runtime.cwasm --target x86-64-v3
//! ```

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use eryx::{CpuFeatureLevel, Session}; // For execute() method and CPU features

/// Pre-compile eryx WASM runtimes for fast sandbox creation.
#[derive(Parser, Debug)]
#[command(name = "eryx-precompile")]
#[command(version, about, long_about = None)]
struct Args {
    /// Input WASM file (.wasm or pre-initialized .wasm)
    #[arg(required = true)]
    input: PathBuf,

    /// Output path (default: input with .cwasm extension, or .wasm for --wasm-only)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Target CPU features for AOT compilation
    ///
    /// Values:
    ///   native     - Host CPU features (default, fastest but not portable)
    ///   x86-64-v3  - AVX2, FMA, BMI1/2 (recommended for Fly.io, no AVX-512)
    ///   x86-64-v2  - SSE4.2, POPCNT (~2008+ CPUs)
    ///   x86-64     - Baseline SSE2 (maximum compatibility)
    ///   `<triple>` - Full target triple (e.g., aarch64-unknown-linux-gnu)
    #[arg(short, long, default_value = "native")]
    target: String,

    /// Pre-initialize Python before compiling
    ///
    /// This runs Python's interpreter initialization and captures the memory state,
    /// reducing session creation time from ~450ms to ~1-5ms.
    #[arg(long)]
    preinit: bool,

    /// Path to Python stdlib (required with --preinit)
    #[arg(long, required_if_eq("preinit", "true"))]
    stdlib: Option<PathBuf>,

    /// Output pre-initialized .wasm instead of native .cwasm
    ///
    /// Use this to create architecture-independent pre-initialized artifacts
    /// that can be AOT compiled later for different targets.
    #[arg(long, conflicts_with = "target")]
    wasm_only: bool,

    /// Skip verification step
    #[arg(long)]
    no_verify: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

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

    println!("eryx-precompile");
    println!("===============");
    println!();
    println!("Input:   {}", args.input.display());
    println!("Output:  {}", output.display());
    if !args.wasm_only {
        println!("Target:  {}", args.target);
    }
    if args.preinit {
        println!(
            "Stdlib:  {}",
            args.stdlib
                .as_ref()
                .map_or("-", |p| p.to_str().unwrap_or("-"))
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
            .expect("--stdlib required with --preinit");

        // Validate stdlib exists
        if !stdlib.exists() || !stdlib.join("encodings").exists() {
            anyhow::bail!(
                "Invalid Python stdlib at {}: expected 'encodings' subdirectory",
                stdlib.display()
            );
        }

        println!();
        println!("Step 1: Pre-initializing Python...");
        let start = Instant::now();

        let preinit_bytes = eryx::preinit::pre_initialize(
            stdlib,
            None, // No site-packages for base runtime
            &[],  // No imports for base runtime
            &[],  // No native extensions for base runtime
        )
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

        // Parse target as either a CPU feature level (x86-64-v3) or target triple
        let (target_triple, cpu_features) = parse_target(&args.target)?;

        let precompiled = eryx::PythonExecutor::precompile_with_options(
            &component_bytes,
            target_triple,
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
            println!("Step 4: Verifying...");
            verify_cwasm(&output, args.stdlib.as_deref()).await?;
            println!("  Verification passed!");
        }
    }

    println!();
    println!("Success!");
    Ok(())
}

/// Parse target string into target triple and CPU feature level.
///
/// CPU feature levels (x86-64, x86-64-v2, x86-64-v3, x86-64-v4, native) are used
/// directly, while full target triples (e.g., aarch64-unknown-linux-gnu) are
/// passed through with Native CPU features.
fn parse_target(target: &str) -> Result<(Option<&str>, CpuFeatureLevel)> {
    // Try to parse as CPU feature level first
    if let Some(level) = CpuFeatureLevel::parse(target) {
        return Ok((None, level));
    }

    // Otherwise treat as target triple with native features
    Ok((Some(target), CpuFeatureLevel::Native))
}

/// Verify that the compiled cwasm file works correctly.
async fn verify_cwasm(cwasm_path: &PathBuf, stdlib: Option<&std::path::Path>) -> Result<()> {
    // For verification, we need the embedded feature which provides EmbeddedResources
    // If stdlib was provided, use that; otherwise try to use embedded resources
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

    if !result.stdout.contains("OK") {
        anyhow::bail!("Verification failed: unexpected output: {}", result.stdout);
    }

    Ok(())
}
