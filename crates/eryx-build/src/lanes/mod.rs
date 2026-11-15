//! Build lanes: language-keyed strategies for producing a WASI component.
//!
//! Each lane consumes a [`Manifest`] and emits a component into the manifest's
//! output dir. The Go lane backs `gh`; the C lane backs `sqlite3` (single)
//! and `curl` (cmake); the Rust lane backs `coreutils`, `rg` and `sd`.

pub mod c;
pub mod go;
pub mod rust;

use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::{Lang, Manifest};

/// Dispatch a build to the lane selected by the manifest's `lang`.
pub fn build(manifest: &Manifest) -> Result<()> {
    match manifest.lang {
        Lang::Go => go::build(manifest),
        Lang::Rust => rust::build(manifest),
        Lang::C => c::build(manifest),
    }
}

/// Convert a path to `&str`, erroring on non-UTF-8 (which our tools can't handle).
pub(crate) fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .with_context(|| format!("path is not valid UTF-8: {}", p.display()))
}

/// Optionally pre-compile a built component to `.cwasm` for fast startup, per
/// the manifest's `[cwasm]` settings. Shared by the lanes; a no-op when
/// `cwasm.enabled` is false. `cwasm.flags` carries any engine knobs the target
/// needs (e.g. the Go lane's async/stackful flags).
pub(crate) fn compile_cwasm(
    manifest: &Manifest,
    component: &Path,
    output_dir: &Path,
    repo_root: &Path,
) -> Result<()> {
    if !manifest.cwasm.enabled {
        return Ok(());
    }
    eprintln!("=== [{}] pre-compile to cwasm ===", manifest.name);
    let cwasm = output_dir.join(format!("{}.cwasm", manifest.name));
    let cwasm_s = path_str(&cwasm)?;
    let component_s = path_str(component)?;
    // An explicit target stops Cranelift from baking in the build machine's
    // CPU features, so the cwasm also loads on other hosts (CI builds it on
    // one runner and runs it on another).
    let target = host_triple();
    let mut args: Vec<&str> = vec!["compile", "--target", &target];
    args.extend(manifest.cwasm.flags.iter().map(String::as_str));
    args.extend([component_s.as_str(), "-o", cwasm_s.as_str()]);
    run("wasmtime", &args, repo_root, &[])?;
    eprintln!("  → {}", cwasm.display());
    Ok(())
}

/// The target triple of the machine running the build, for `wasmtime compile`.
fn host_triple() -> String {
    use std::env::consts::{ARCH, OS};
    match OS {
        "linux" => format!("{ARCH}-unknown-linux-gnu"),
        "macos" => format!("{ARCH}-apple-darwin"),
        "windows" => format!("{ARCH}-pc-windows-msvc"),
        os => format!("{ARCH}-unknown-{os}"),
    }
}

/// Run a subprocess, returning an error if it exits non-zero.
///
/// Shared by the lanes; logs the command before running it.
pub(crate) fn run(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
    envs: &[(&str, &str)],
) -> Result<()> {
    use anyhow::bail;

    let pretty_env: String = envs
        .iter()
        .map(|(k, v)| format!("{k}={v} "))
        .collect::<String>();
    eprintln!(
        "  $ (cd {}) {pretty_env}{program} {}",
        cwd.display(),
        args.join(" ")
    );

    let mut cmd = std::process::Command::new(program);
    cmd.args(args).current_dir(cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `{program}` (is it on PATH?)"))?;
    if !status.success() {
        bail!("`{program}` exited with {status}");
    }
    Ok(())
}
