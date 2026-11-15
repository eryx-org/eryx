//! Go lane: builds a CLI to wasip3 using the experimental Go fork, then
//! componentizes with `wasm-tools`.
//!
//! The fork targets WASI 0.3.0, as implemented by the wasmtime version eryx
//! uses. Its repo and commit are pinned in `scripts/bootstrap-go-wasip3.sh`.
//!
//! Toolchain inputs (not per-CLI, so sourced from the environment):
//! - `ERYX_GO_WASIP3_ROOT` — GOROOT of the wasip3 Go fork
//!   (default `scratch/go-wasip3`). The WIT dir is derived from it.
//! - `wasm-tools` and `wasmtime` are taken from `PATH` (mise provides them).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::manifest::Manifest;

/// Default location of the wasip3 Go fork, relative to the repo root.
const DEFAULT_GOROOT: &str = "scratch/go-wasip3";

pub fn build(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;

    // Resolve the wasip3 Go toolchain.
    let goroot = match std::env::var_os("ERYX_GO_WASIP3_ROOT") {
        Some(p) => PathBuf::from(p),
        None => repo_root.join(DEFAULT_GOROOT),
    };
    let go_bin = goroot.join("bin/go");
    if !go_bin.exists() {
        bail!(
            "wasip3 Go toolchain not found at {} — build it with \
             scripts/bootstrap-go-wasip3.sh or set ERYX_GO_WASIP3_ROOT",
            go_bin.display()
        );
    }
    let wit_dir = goroot.join("src/internal/wasi/wit");
    if !wit_dir.exists() {
        bail!("WIT dir not found at {}", wit_dir.display());
    }

    let source_dir = repo_root.join(&manifest.source.dir);
    let output_dir = repo_root.join(&manifest.output.dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    let goroot_s = path_str(&goroot)?;
    let go_bin_s = path_str(&go_bin)?;
    let wit_s = path_str(&wit_dir)?;
    let out_wasm = output_dir.join(format!("{}.wasm", manifest.name));
    let out_wasm_s = path_str(&out_wasm)?;

    eprintln!("=== [{}] Step 1: vendor dependencies ===", manifest.name);
    run(
        &go_bin_s,
        &["mod", "vendor"],
        &source_dir,
        &[("GOROOT", &goroot_s)],
    )?;

    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] Step 2: patch vendor tree ===", manifest.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!("vendor_patch script {} not found", patch_abs.display());
        }
        let patch_s = path_str(&patch_abs)?;
        run("bash", &[&patch_s], &source_dir, &[])?;
    }

    eprintln!(
        "=== [{}] Step 3: compile to wasm32 (wasip3) ===",
        manifest.name
    );
    let ldflags = manifest
        .build
        .ldflags
        .replace("{ref}", &manifest.source.git_ref)
        .replace("{version}", manifest.source.git_ref.trim_start_matches('v'));
    let mut args = vec!["build", "-mod=vendor", "-o", &out_wasm_s];
    if !ldflags.is_empty() {
        args.extend(["-ldflags", &ldflags]);
    }
    args.push(&manifest.build.package);
    run(
        &go_bin_s,
        &args,
        &source_dir,
        &[
            ("GOROOT", &goroot_s),
            ("GOOS", "wasip3"),
            ("GOARCH", "wasm32"),
        ],
    )?;

    eprintln!("=== [{}] Step 4: componentize ===", manifest.name);
    let embedded = output_dir.join("embedded.wasm");
    let component = output_dir.join("component.wasm");
    let embedded_s = path_str(&embedded)?;
    let component_s = path_str(&component)?;
    run(
        "wasm-tools",
        &[
            "component",
            "embed",
            "--all-features",
            &wit_s,
            "--world",
            &manifest.component.world,
            &out_wasm_s,
            "-o",
            &embedded_s,
        ],
        &repo_root,
        &[],
    )?;
    run(
        "wasm-tools",
        &["component", "new", &embedded_s, "-o", &component_s],
        &repo_root,
        &[],
    )?;
    std::fs::remove_file(&embedded).ok();

    compile_cwasm(manifest, &component, &output_dir, &repo_root)?;

    eprintln!("=== [{}] done → {} ===", manifest.name, component.display());
    Ok(())
}
