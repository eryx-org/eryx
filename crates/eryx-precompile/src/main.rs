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
//!
//! # Pre-init with packages (wheels, tar.gz, or directories)
//! eryx-precompile runtime.wasm -o runtime-numpy.cwasm --preinit --stdlib ./python-stdlib \
//!   --package numpy-2.2.3-wasi.tar.gz --import numpy
//!
//! # Pre-init with site-packages directory
//! eryx-precompile runtime.wasm -o runtime.cwasm --preinit --stdlib ./python-stdlib \
//!   --site-packages ./my-site-packages --import jinja2
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

    /// Path to a site-packages directory
    ///
    /// Mount a directory containing Python packages. Any .so files found
    /// will be linked as native extensions. Can be combined with --package.
    #[arg(long)]
    site_packages: Option<PathBuf>,

    /// Package file to include (.whl, .tar.gz, or directory)
    ///
    /// Packages are extracted and their contents are made available as
    /// site-packages. Native extensions (.so files) are automatically
    /// linked. Can be specified multiple times.
    #[arg(long = "package", value_name = "PATH")]
    packages: Vec<PathBuf>,

    /// Module to pre-import during initialization
    ///
    /// Pre-importing modules captures their initialized state in the snapshot,
    /// making them instantly available at runtime. Can be specified multiple times.
    ///
    /// Example: --import numpy --import pandas
    #[arg(long = "import", value_name = "MODULE")]
    imports: Vec<String>,

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
        let (final_site_packages, extensions, _extracted_packages) =
            process_packages(args.site_packages.as_ref(), &args.packages)?;

        if let Some(ref sp) = final_site_packages {
            println!("Site-packages dir: {}", sp.display());
        }
        if !extensions.is_empty() {
            println!("Native extensions: {}", extensions.len());
        }

        // Convert imports to &str references
        let import_refs: Vec<&str> = args.imports.iter().map(|s| s.as_str()).collect();

        println!();
        println!("Step 1: Pre-initializing Python...");
        let start = Instant::now();

        let preinit_bytes = eryx::preinit::pre_initialize(
            stdlib,
            final_site_packages.as_deref(),
            &import_refs,
            &extensions,
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
            verify_cwasm(&output, args.stdlib.as_deref(), &args.imports).await?;
            println!("  Verification passed!");
        }
    }

    println!();
    println!("Success!");
    Ok(())
}

/// Process packages and site-packages to extract native extensions.
///
/// Returns (site_packages_path, native_extensions, extracted_packages).
/// The extracted_packages must be kept alive to prevent temp directory cleanup.
fn process_packages(
    site_packages: Option<&PathBuf>,
    packages: &[PathBuf],
) -> Result<(
    Option<PathBuf>,
    Vec<eryx::preinit::NativeExtension>,
    Vec<eryx::ExtractedPackage>,
)> {
    let mut extensions = Vec::new();
    let mut extracted_packages = Vec::new();
    let mut final_site_packages = site_packages.cloned();

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

        // Use the first package's python_path as site_packages if not already set
        if final_site_packages.is_none() {
            final_site_packages = Some(package.python_path.clone());
        } else if let Some(ref target_dir) = final_site_packages {
            // Copy this package's contents into the consolidated site-packages directory
            copy_directory_contents(&package.python_path, target_dir)
                .with_context(|| format!("Failed to merge package: {}", package.name))?;
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
                let dlopen_path = format!("/site-packages/{}", relative.display());

                // Skip if we already have this extension from packages
                if extensions.iter().any(|e| e.name == dlopen_path) {
                    continue;
                }

                let bytes = std::fs::read(path).context("Failed to read native extension .so")?;
                extensions.push(eryx::preinit::NativeExtension::new(dlopen_path, bytes));
            }
        }
    }

    Ok((final_site_packages, extensions, extracted_packages))
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
async fn verify_cwasm(
    cwasm_path: &PathBuf,
    stdlib: Option<&std::path::Path>,
    imports: &[String],
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

    if !result.stdout.contains("OK") {
        anyhow::bail!("Verification failed: unexpected output: {}", result.stdout);
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

        if !result.stdout.contains("imports OK") {
            let error_detail = if result.stderr.is_empty() {
                result.stdout.clone()
            } else {
                result.stderr.clone()
            };
            anyhow::bail!(
                "Import verification failed for [{}]: {}",
                imports.join(", "),
                error_detail
            );
        }

        println!("  Imports verified: {}", imports.join(", "));
    }

    Ok(())
}
