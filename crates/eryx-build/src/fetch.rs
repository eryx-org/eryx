//! Fetching pinned upstream sources into the gitignored `scratch/` dir.
//!
//! The manifest's URL and ref are the only pin: a `.git` URL is shallow-cloned
//! at `ref` (a tag or branch), and a `.zip` or `.tar.*` URL is downloaded and
//! unpacked. A dir that already exists is left alone, so delete it to re-fetch
//! after bumping a ref.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::lanes::{path_str, run};
use crate::manifest::Manifest;

/// Fetch the manifest's source and each `[[deps]]` source that is missing.
pub fn sources(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;
    fetch(
        &manifest.source.repo,
        &manifest.source.git_ref,
        &repo_root.join(&manifest.source.dir),
    )?;
    for dep in &manifest.deps {
        fetch(&dep.url, &dep.git_ref, &repo_root.join(&dep.dir))?;
    }
    Ok(())
}

fn fetch(url: &str, git_ref: &str, dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    eprintln!("=== fetch {url} @ {git_ref} → {} ===", dir.display());
    // Unpack next to the final dir and rename at the end, so an interrupted
    // fetch doesn't leave a half-populated dir that later runs would skip.
    let tmp = dir.with_extension("fetching");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    let parent = dir.parent().context("source dir has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp_s = path_str(&tmp)?;

    if url.ends_with(".git") {
        run(
            "git",
            &["clone", "--depth", "1", "--branch", git_ref, url, &tmp_s],
            parent,
            &[],
        )?;
    } else if url.ends_with(".zip") || url.contains(".tar.") {
        std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let archive = tmp.join("download");
        let archive_s = path_str(&archive)?;
        run("curl", &["-fsSL", "-o", &archive_s, url], parent, &[])?;
        if url.ends_with(".zip") {
            // Flatten: the files the manifests name sit at the top level.
            run(
                "unzip",
                &["-q", "-j", &archive_s, "-d", &tmp_s],
                parent,
                &[],
            )?;
        } else {
            run(
                "tar",
                &["xf", &archive_s, "--strip-components=1", "-C", &tmp_s],
                parent,
                &[],
            )?;
        }
        std::fs::remove_file(&archive).ok();
    } else {
        bail!("don't know how to fetch {url}: expected a .git, .zip or .tar.* URL");
    }

    std::fs::rename(&tmp, dir)
        .with_context(|| format!("renaming {} → {}", tmp.display(), dir.display()))
}
