//! Provenance written next to each built component, so the component can be
//! published (or embedded) without the eryx checkout that built it.
//!
//! The output dir ends up holding:
//! - `component.wasm` (and `<name>.cwasm` when `[cwasm]` is enabled);
//! - `manifest.toml`, a copy of the manifest the build used;
//! - `provenance.json`, described by [`Provenance`].

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::manifest::Manifest;

/// Version of the `provenance.json` layout. Bump it on incompatible changes.
const FORMAT: u32 = 1;

/// The contents of `provenance.json`.
#[derive(Debug, Serialize)]
struct Provenance<'a> {
    format: u32,
    name: &'a str,
    description: &'a str,
    /// SPDX expression for the upstream source.
    license: &'a str,
    lang: String,
    source: Upstream<'a>,
    /// Libraries linked into the component (C lane `[[deps]]`).
    deps: Vec<Upstream<'a>>,
    /// Patches and shims applied, repo-root-relative, with their sha256.
    patches: Vec<File>,
    /// Built artifacts in the output dir, with their sha256.
    artifacts: Vec<File>,
}

#[derive(Debug, Serialize)]
struct Upstream<'a> {
    name: &'a str,
    url: &'a str,
    /// The pinned ref from the manifest.
    #[serde(rename = "ref")]
    git_ref: &'a str,
    /// The commit `ref` resolved to, for git sources.
    commit: Option<String>,
    license: &'a str,
}

#[derive(Debug, Serialize)]
struct File {
    path: String,
    sha256: String,
}

/// Write `manifest.toml` and `provenance.json` into the manifest's output dir.
pub fn write(manifest: &Manifest, manifest_path: &Path) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;
    let output_dir = repo_root.join(&manifest.output.dir);

    std::fs::copy(manifest_path, output_dir.join("manifest.toml"))
        .with_context(|| format!("copying {}", manifest_path.display()))?;

    let patches = manifest
        .build
        .vendor_patch
        .iter()
        .chain(
            manifest
                .deps
                .iter()
                .flat_map(|d| d.config_patch.iter().chain(&d.shims)),
        )
        .map(|p| file(&repo_root, p))
        .collect::<Result<_>>()?;

    let mut names = vec!["component.wasm".to_string()];
    if manifest.cwasm.enabled {
        names.push(format!("{}.cwasm", manifest.name));
    }
    let artifacts = names
        .iter()
        .map(|n| file(&output_dir, Path::new(n)))
        .collect::<Result<_>>()?;

    let provenance = Provenance {
        format: FORMAT,
        name: &manifest.name,
        description: &manifest.description,
        license: &manifest.license,
        lang: format!("{:?}", manifest.lang).to_lowercase(),
        source: Upstream {
            name: &manifest.name,
            url: &manifest.source.repo,
            git_ref: &manifest.source.git_ref,
            commit: commit(&repo_root.join(&manifest.source.dir)),
            license: &manifest.license,
        },
        deps: manifest
            .deps
            .iter()
            .map(|d| Upstream {
                name: &d.name,
                url: &d.url,
                git_ref: &d.git_ref,
                commit: commit(&repo_root.join(&d.dir)),
                license: &d.license,
            })
            .collect(),
        patches,
        artifacts,
    };
    let out = output_dir.join("provenance.json");
    let json = serde_json::to_string_pretty(&provenance)?;
    std::fs::write(&out, json + "\n").with_context(|| format!("writing {}", out.display()))?;
    eprintln!("  → {}", out.display());
    Ok(())
}

/// Hash `base/rel`, recording it under `rel`.
fn file(base: &Path, rel: &Path) -> Result<File> {
    let path = base.join(rel);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(File {
        path: rel.to_string_lossy().into_owned(),
        sha256: hex::encode(Sha256::digest(&bytes)),
    })
}

/// The checked-out commit of a git source dir, or `None` for archives.
fn commit(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
