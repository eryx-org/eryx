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
    /// The repo files the build applies, compiles or links (patches, shims,
    /// WIT; see [`Manifest::repo_inputs`]), repo-root-relative, with their
    /// sha256. A WIT directory is listed file by file.
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
    /// Absent for a local `path` source.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    /// The pinned ref from the manifest.
    #[serde(rename = "ref")]
    git_ref: &'a str,
    /// The pinned commit, for git sources; for a `path` source that's a git
    /// checkout, its `HEAD` (not a pin).
    commit: Option<String>,
    /// The pinned sha256 of the download, for archives.
    sha256: Option<&'a str>,
    license: &'a str,
    /// A local `path` source's absolute path. Its presence marks the build
    /// unpinned: publishing tools must refuse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// For a `path` source: true unless it's a git checkout with no
    /// uncommitted changes (`git status --porcelain` empty). A clean
    /// checkout's `commit` may still be unpushed.
    #[serde(skip_serializing_if = "Option::is_none")]
    dirty: Option<bool>,
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

    let patches = patches(manifest, &repo_root)?;

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
        source: source(manifest),
        deps: manifest
            .deps
            .iter()
            .map(|d| Upstream {
                name: &d.name,
                url: Some(&d.url),
                git_ref: &d.git_ref,
                commit: d.commit.clone(),
                sha256: d.sha256.as_deref(),
                license: &d.license,
                path: None,
                dirty: None,
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

/// The `source` entry: the manifest's pin, or for a local `path` source its
/// path, git `HEAD` and whether it's dirty.
fn source(manifest: &Manifest) -> Upstream<'_> {
    let src = &manifest.source;
    let mut upstream = Upstream {
        name: &manifest.name,
        url: Some(&src.repo),
        git_ref: &src.git_ref,
        commit: src.commit.clone(),
        sha256: src.sha256.as_deref(),
        license: &manifest.license,
        path: None,
        dirty: None,
    };
    if let Some(path) = &src.path {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        // A tree inside a git checkout (`path` may be a subdirectory).
        let head = git(&["rev-parse", "HEAD"]);
        let status = git(&["status", "--porcelain", "--", "."]);
        upstream.url = None;
        upstream.dirty = Some(status.is_none_or(|s| !s.is_empty()));
        upstream.commit = head;
        upstream.path = Some(path.to_string_lossy().into_owned());
    }
    upstream
}

/// The `patches` entries: every [`Manifest::repo_inputs`] file, hashed.
fn patches(manifest: &Manifest, repo_root: &Path) -> Result<Vec<File>> {
    let mut out = Vec::new();
    for rel in manifest.repo_inputs() {
        files(repo_root, rel, &mut out)?;
    }
    Ok(out)
}

/// Hash `base/rel` into `out`: the file itself, or every file under it,
/// sorted, when it's a directory.
fn files(base: &Path, rel: &Path, out: &mut Vec<File>) -> Result<()> {
    let path = base.join(rel);
    if !path.is_dir() {
        out.push(file(base, rel)?);
        return Ok(());
    }
    let mut entries = std::fs::read_dir(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .map(|e| Ok(rel.join(e?.file_name())))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("reading {}", path.display()))?;
    entries.sort();
    for entry in entries {
        files(base, &entry, out)?;
    }
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
        .context(
            "failed to spawn `wasm-tools`, which every lane needs for provenance.json \
             (install it, e.g. `cargo install wasm-tools`, and put it on PATH)",
        )?;
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
/// component's WIT, as `ns:pkg@ver` (or `ns:pkg` when unversioned), sorted
/// and without duplicates. Imports
/// inside the `package` blocks that follow (`use` and nested worlds) aren't
/// the component's own, so only the first world's lines count.
fn imported_packages(wit: &str) -> Vec<String> {
    let world = wit.split_once("world ").map_or("", |(_, rest)| {
        rest.split_once('}').map_or(rest, |(w, _)| w)
    });
    let mut packages: Vec<String> = world
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import ")?.strip_suffix(';'))
        // `ns:pkg/iface[@ver]` names a package; `name: func(...)` and other
        // local imports don't.
        .filter(|name| name.contains(':') && !name.contains(char::is_whitespace))
        .map(|name| {
            let (path, version) = name
                .split_once('@')
                .map_or((name, None), |(p, v)| (p, Some(v)));
            let package = path.split_once('/').map_or(path, |(p, _)| p);
            match version {
                Some(version) => format!("{package}@{version}"),
                None => package.to_string(),
            }
        })
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `patches` records the build's own shims and WIT (a directory, file by
    /// file) alongside the vendor patch and the deps' patches and shims.
    #[test]
    fn patches_cover_every_repo_input() {
        let root = std::env::temp_dir().join(format!("eryx-build-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, body) in [
            ("patch.sh", "p"),
            ("shims/stubs.c", "s"),
            ("wit/process.wit", "w"),
            ("wit/deps/io.wit", "d"),
            ("dep-patch.sh", "dp"),
            ("dep-shim.c", "ds"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let manifest: Manifest = toml::from_str(
            r#"
            name = "git"
            license = "GPL-2.0-only"
            lang = "c"
            [source]
            repo = "https://example.com/git.git"
            ref = "v2"
            commit = "0000000000000000000000000000000000000000"
            dir = "scratch/git"
            [output]
            dir = "out/git"
            [build]
            vendor_patch = "patch.sh"
            shims = ["shims/stubs.c"]
            wit = { path = "wit", world = "eryx:process/child" }
            [[deps]]
            name = "zlib"
            url = "https://example.com/zlib.tar.gz"
            ref = "1"
            sha256 = "00"
            license = "Zlib"
            dir = "scratch/zlib"
            config_patch = "dep-patch.sh"
            shims = ["dep-shim.c"]
            "#,
        )
        .unwrap();
        let got = patches(&manifest, &root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        let paths: Vec<_> = got.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "patch.sh",
                "shims/stubs.c",
                "wit/deps/io.wit",
                "wit/process.wit",
                "dep-patch.sh",
                "dep-shim.c",
            ]
        );
        assert_eq!(got[1].sha256, hex::encode(Sha256::digest(b"s")));
    }

    /// A `path` source is recorded with its path, no URL, its git `HEAD`
    /// and whether it has uncommitted changes, which the build's own
    /// `target/` counts as unless ignored.
    #[test]
    fn path_source_is_recorded_as_local() {
        let root =
            std::env::temp_dir().join(format!("eryx-build-prov-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tool")).unwrap();
        let manifest_path = root.join("tool.toml");
        std::fs::write(
            &manifest_path,
            "name = \"tool\"\nlicense = \"MIT\"\nlang = \"rust\"\n\
             [source]\npath = \"tool\"\n[build]\nbin = \"tool\"\n[output]\ndir = \"out\"\n",
        )
        .unwrap();
        let record = || {
            let manifest = Manifest::load(&manifest_path).unwrap();
            serde_json::to_value(source(&manifest)).unwrap()
        };

        // Not a git checkout: no commit, dirty.
        let v = record();
        let abs = std::fs::canonicalize(root.join("tool")).unwrap();
        assert_eq!(v["path"], abs.to_string_lossy().as_ref());
        assert!(v.get("url").is_none(), "{v}");
        assert_eq!(v["commit"], serde_json::Value::Null);
        assert_eq!(v["dirty"], true);

        // A clean checkout records HEAD, not dirty; an edit makes it dirty.
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(root.join("tool"))
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(root.join("tool/main.rs"), "fn main() {}").unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "x"]);
        let v = record();
        assert_eq!(v["commit"].as_str().map(str::len), Some(40), "{v}");
        assert_eq!(v["dirty"], false);
        std::fs::write(root.join("tool/main.rs"), "fn main() { }").unwrap();
        assert_eq!(record()["dirty"], true);
        std::fs::remove_dir_all(&root).unwrap();
    }

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
  import my:host/log;

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
                "my:host",
                "wasi:cli@0.2.12",
                "wasi:io@0.2.12",
                "wasi:sockets@0.3.0-rc-2026-03-15",
            ]
        );
        assert!(imported_packages("").is_empty());
    }
}
