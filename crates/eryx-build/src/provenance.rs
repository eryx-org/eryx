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
    /// Version of the eryx-build that made the component.
    eryx_build: &'static str,
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
    /// The WIT packages `component.wasm` imports, with their versions
    /// (`wasi:cli@0.2.12`, `eryx:process@0.1.0`), sorted: what a host needs
    /// to provide to run it.
    imports: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Upstream<'a> {
    name: &'a str,
    url: &'a str,
    /// The pinned ref from the manifest.
    #[serde(rename = "ref")]
    git_ref: &'a str,
    /// The pinned commit, for git sources.
    commit: Option<&'a str>,
    /// The pinned sha256 of the download, for archives.
    sha256: Option<&'a str>,
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
    let imports = imports(&output_dir.join("component.wasm"))?;

    let provenance = Provenance {
        format: FORMAT,
        eryx_build: env!("CARGO_PKG_VERSION"),
        name: &manifest.name,
        description: &manifest.description,
        license: &manifest.license,
        lang: format!("{:?}", manifest.lang).to_lowercase(),
        source: Upstream {
            name: &manifest.name,
            url: &manifest.source.repo,
            git_ref: &manifest.source.git_ref,
            commit: manifest.source.commit.as_deref(),
            sha256: manifest.source.sha256.as_deref(),
            license: &manifest.license,
        },
        deps: manifest
            .deps
            .iter()
            .map(|d| Upstream {
                name: &d.name,
                url: &d.url,
                git_ref: &d.git_ref,
                commit: d.commit.as_deref(),
                sha256: d.sha256.as_deref(),
                license: &d.license,
            })
            .collect(),
        patches,
        artifacts,
        imports,
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

/// The WIT packages `component` imports, from `wasm-tools component wit`.
fn imports(component: &Path) -> Result<Vec<String>> {
    let out = std::process::Command::new("wasm-tools")
        .args(["component", "wit"])
        .arg(component)
        .output()
        .context("failed to spawn `wasm-tools` (is it on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "`wasm-tools component wit {}` failed: {}",
            component.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(imported_packages(&String::from_utf8_lossy(&out.stdout)))
}

/// The packages named by the `import ns:pkg/iface@ver;` lines of a
/// component's WIT, as `ns:pkg@ver`, sorted and without duplicates. Imports
/// inside the `package` blocks that follow (`use` and nested worlds) aren't
/// the component's own, so only the first world's lines count.
fn imported_packages(wit: &str) -> Vec<String> {
    let world = wit.split_once("world ").map_or("", |(_, rest)| {
        rest.split_once('}').map_or(rest, |(w, _)| w)
    });
    let mut packages: Vec<String> = world
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import ")?.strip_suffix(';'))
        .filter_map(|name| {
            let (path, version) = name.split_once('@')?;
            let package = path.split_once('/').map_or(path, |(p, _)| p);
            Some(format!("{package}@{version}"))
        })
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_packages_come_from_the_component_world() {
        let wit = "package root:component;

world root {
  import wasi:cli/environment@0.2.12;
  import wasi:io/streams@0.2.12;
  import wasi:cli/stdout@0.2.12;
  import eryx:process/run@0.1.0;
  import wasi:sockets/types@0.3.0-rc-2026-03-15;
  import local-thing: func();

  export wasi:cli/run@0.2.12;
}
package wasi:io@0.2.12 {
  interface streams {
    use wasi:clocks/types@0.2.12.{duration};
  }
  world imports {
    import wasi:random/random@0.2.12;
  }
}
";
        assert_eq!(
            imported_packages(wit),
            [
                "eryx:process@0.1.0",
                "wasi:cli@0.2.12",
                "wasi:io@0.2.12",
                "wasi:sockets@0.3.0-rc-2026-03-15",
            ]
        );
        assert!(imported_packages("").is_empty());
    }
}
