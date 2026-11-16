//! `eryx-build`: a manifest-driven, multi-lane build driver for the CLI
//! components eryx runs as child commands.
//!
//! Reads a per-CLI manifest (`clis/<name>.toml`) and dispatches to the lane
//! selected by its `lang` field (rust | c | go). Adding a CLI should be a new
//! manifest, not a bespoke script.
//!
//! Usage:
//!
//! ```text
//! eryx-build <name>          Build clis/<name>.toml (e.g. `eryx-build sd`)
//! eryx-build <path.toml>     Build a manifest at an explicit path
//! eryx-build --list          List available manifests in clis/
//! eryx-build --check         Validate all manifests without building (CI gate)
//! ```
//!
//! Run from the repo root (the mise `build-cli` task does this). A missing
//! source or dependency dir is fetched from the manifest's pinned URL first,
//! and the output dir gets a provenance record next to the component (see
//! `provenance`).
//! Toolchains (`cargo`, `wasm-tools`, `wasmtime`, `git`, `curl`) come from
//! PATH; the C lane also needs `WASI_SDK_PATH`, and the Go lane the wasip3 Go
//! fork (see `lanes::go`).

mod fetch;
mod lanes;
mod manifest;
mod provenance;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::manifest::Manifest;

/// Directory holding per-CLI manifests, relative to the repo root.
const CLIS_DIR: &str = "clis";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("eryx-build: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => {
            print_usage();
            Ok(())
        }
        Some("--list") => list_manifests(),
        Some("--check") => check_all_manifests(),
        Some(arg) => {
            let path = resolve_manifest_path(arg);
            let manifest = Manifest::load(&path)?;
            eprintln!(
                "eryx-build: building '{}' ({}) via the {:?} lane, {} @ {}",
                manifest.name,
                manifest.description,
                manifest.lang,
                manifest.source.repo,
                manifest.source.git_ref,
            );
            fetch::sources(&manifest)?;
            lanes::build(&manifest)?;
            provenance::write(&manifest, &path)
        }
    }
}

/// Map a CLI argument to a manifest path: a `.toml` path is used as-is,
/// otherwise it's treated as a name under `clis/`.
fn resolve_manifest_path(arg: &str) -> PathBuf {
    if arg.ends_with(".toml") {
        PathBuf::from(arg)
    } else {
        PathBuf::from(CLIS_DIR).join(format!("{arg}.toml"))
    }
}

/// Collect the paths of all `*.toml` manifests in `clis/`, and in each
/// directory of `clis/gpl/` (GPL-licensed recipes, kept apart; #837), sorted.
fn manifest_paths() -> Result<Vec<PathBuf>> {
    let dir = PathBuf::from(CLIS_DIR);
    let tomls = |dir: &std::path::Path| -> Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(dir).with_context(|| {
            format!(
                "reading manifest dir {} (run eryx-build from the repo root)",
                dir.display()
            )
        })?;
        Ok(entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect())
    };
    let mut paths = tomls(&dir)?;
    if let Ok(gpl) = std::fs::read_dir(dir.join("gpl")) {
        for entry in gpl.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            paths.extend(tomls(&entry.path())?);
        }
    }
    paths.sort();
    if paths.is_empty() {
        bail!("no manifests found in {}", dir.display());
    }
    Ok(paths)
}

fn list_manifests() -> Result<()> {
    eprintln!("Available CLI manifests:");
    for path in manifest_paths()? {
        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
            println!("  {name}");
        }
    }
    Ok(())
}

/// Validate every manifest in `clis/` without building: parse it, and check
/// that every patch and shim it references exists. Needs no toolchains, so CI
/// can run it on every change to catch manifest bit-rot.
fn check_all_manifests() -> Result<()> {
    let paths = manifest_paths()?;
    let mut failures = 0usize;
    for path in &paths {
        match check_one(path) {
            Ok(manifest) => eprintln!(
                "  ok   {}: {:?} lane, {} @ {}",
                path.display(),
                manifest.lang,
                manifest.source.repo,
                manifest.source.git_ref
            ),
            Err(e) => {
                eprintln!("  FAIL {}: {e:#}", path.display());
                failures += 1;
            }
        }
    }
    if failures > 0 {
        bail!("{failures} of {} manifest(s) invalid", paths.len());
    }
    eprintln!("All {} manifest(s) valid.", paths.len());
    Ok(())
}

/// Parse one manifest and validate cheap, toolchain-free invariants.
fn check_one(path: &std::path::Path) -> Result<Manifest> {
    let manifest = Manifest::load(path)?;
    let src = &manifest.source;
    fetch::pin(&src.repo, src.commit.as_deref(), src.sha256.as_deref())?;
    for dep in &manifest.deps {
        fetch::pin(&dep.url, dep.commit.as_deref(), dep.sha256.as_deref())?;
    }
    // Patches and shims are tracked in-repo, so their existence is checkable.
    // Source dirs live in the gitignored scratch/, so they aren't.
    let tracked = manifest
        .build
        .vendor_patch
        .iter()
        .chain(&manifest.build.shims)
        .chain(manifest.build.wit.iter().map(|w| &w.path))
        .chain(
            manifest
                .deps
                .iter()
                .flat_map(|d| d.config_patch.iter().chain(&d.shims)),
        );
    for file in tracked {
        if !file.exists() {
            bail!("{} does not exist", file.display());
        }
    }
    Ok(manifest)
}

fn print_usage() {
    eprintln!(
        "eryx-build: manifest-driven CLI component builder\n\n\
         Usage:\n  \
         eryx-build <name>        Build clis/<name>.toml\n  \
         eryx-build <path.toml>   Build a manifest at an explicit path\n  \
         eryx-build --list        List available manifests\n  \
         eryx-build --check       Validate all manifests (no build)"
    );
}
