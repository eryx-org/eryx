//! Rust lane: builds a CLI to a **wasip2 component** via `cargo build
//! --target wasm32-wasip2`.
//!
//! The simplest lane: since Rust 1.82 the `wasm32-wasip2` target emits a
//! component directly (no `wasm-tools` step, no wasi-sdk). The spike candidate
//! (ripgrep) built and ran clean with no source patches or special flags —
//! `cargo build --release --target wasm32-wasip2 --bin <bin>` and the artifact
//! is `target/wasm32-wasip2/release/<bin>.wasm`.
//!
//! The `wasm32-wasip2` rustup target must be installed
//! (`rustup target add wasm32-wasip2`). cargo comes from PATH.

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::manifest::Manifest;

pub fn build(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;

    let bin = manifest.build.bin.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "the Rust lane needs `[build] bin = \"...\"` (the cargo binary) for '{}'",
            manifest.name
        )
    })?;

    let source_dir = repo_root.join(&manifest.source.dir);
    let output_dir = repo_root.join(&manifest.output.dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    // Optional patch hook (mirrors the other lanes), applied before building.
    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] patch source tree ===", manifest.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!("vendor_patch script {} not found", patch_abs.display());
        }
        let patch_s = path_str(&patch_abs)?;
        run("bash", &[&patch_s], &source_dir, &[])?;
    }

    eprintln!("=== [{}] cargo build → wasip2 component ===", manifest.name);
    let mut args: Vec<String> = vec![
        "build".into(),
        "--release".into(),
        "--target".into(),
        "wasm32-wasip2".into(),
        "--bin".into(),
        bin.to_string(),
    ];
    args.extend(manifest.build.cargo_flags.iter().cloned());
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    // Pin the target dir to the source tree: a `.cargo/config.toml` or
    // CARGO_TARGET_DIR inherited from the eryx checkout would otherwise send
    // the build (and the artifact this lane looks for) somewhere else.
    let target_dir = source_dir.join("target");
    let target_dir_s = path_str(&target_dir)?;
    let mut envs: Vec<(&str, &str)> = vec![("CARGO_TARGET_DIR", &target_dir_s)];
    envs.extend(
        manifest
            .build
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    run("cargo", &args_ref, &source_dir, &envs)?;

    let artifact = target_dir
        .join("wasm32-wasip2/release")
        .join(format!("{bin}.wasm"));
    if !artifact.exists() {
        bail!(
            "expected cargo artifact {} not found after build",
            artifact.display()
        );
    }
    let component = output_dir.join("component.wasm");
    std::fs::copy(&artifact, &component)
        .with_context(|| format!("copying {} → {}", artifact.display(), component.display()))?;
    eprintln!("  {} → {}", artifact.display(), component.display());

    compile_cwasm(manifest, &component, &output_dir, &repo_root)?;

    eprintln!("=== [{}] done → {} ===", manifest.name, component.display());
    Ok(())
}
