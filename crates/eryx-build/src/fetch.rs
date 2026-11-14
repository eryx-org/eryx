//! Fetching pinned upstream sources into the gitignored `scratch/` dir.
//!
//! Each source is pinned by content, not by its `ref` label: a `.git` URL by
//! `commit` (fetched at that commit, shallowly), and a `.zip` or `.tar.*` URL
//! by the `sha256` of the download. A dir that already exists is checked
//! against the pin rather than re-fetched, so a stale checkout left over from
//! an older pin fails the build: delete the dir to re-fetch.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::lanes::{path_str, run};
use crate::manifest::Manifest;

/// Records an archive's sha256 inside its unpacked dir, so a later run can
/// check an existing dir without the download.
const ARCHIVE_MARKER: &str = ".eryx-sha256";

/// What a source is pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin<'a> {
    /// A git commit (40 hex digits).
    Git(&'a str),
    /// The sha256 of an archive download (64 hex digits).
    Archive(&'a str),
}

/// Work out a source's pin from its URL: `.git` URLs need `commit`, archives
/// need `sha256`, and setting the other one is an error.
pub fn pin<'a>(url: &str, commit: Option<&'a str>, sha256: Option<&'a str>) -> Result<Pin<'a>> {
    let is_hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    if url.ends_with(".git") {
        ensure!(
            sha256.is_none(),
            "{url}: a git source is pinned by `commit`, not `sha256`"
        );
        let commit = commit.with_context(|| format!("{url}: missing `commit`"))?;
        ensure!(
            is_hex(commit, 40),
            "{url}: `commit` must be a full 40-digit hash"
        );
        Ok(Pin::Git(commit))
    } else if url.ends_with(".zip") || url.contains(".tar.") {
        ensure!(
            commit.is_none(),
            "{url}: an archive is pinned by `sha256`, not `commit`"
        );
        let sha256 = sha256.with_context(|| format!("{url}: missing `sha256`"))?;
        ensure!(is_hex(sha256, 64), "{url}: `sha256` must be 64 hex digits");
        Ok(Pin::Archive(sha256))
    } else {
        bail!("don't know how to fetch {url}: expected a .git, .zip or .tar.* URL")
    }
}

/// Fetch (or check) the manifest's source and each `[[deps]]` source.
pub fn sources(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;
    let src = &manifest.source;
    fetch(
        &src.repo,
        pin(&src.repo, src.commit.as_deref(), src.sha256.as_deref())?,
        &repo_root.join(&src.dir),
    )?;
    for dep in &manifest.deps {
        fetch(
            &dep.url,
            pin(&dep.url, dep.commit.as_deref(), dep.sha256.as_deref())?,
            &repo_root.join(&dep.dir),
        )?;
    }
    Ok(())
}

fn fetch(url: &str, pin: Pin<'_>, dir: &Path) -> Result<()> {
    if dir.exists() {
        return verify_existing(dir, pin);
    }
    eprintln!("=== fetch {url} @ {pin:?} → {} ===", dir.display());
    // Unpack next to the final dir and rename at the end, so an interrupted
    // fetch doesn't leave a half-populated dir behind.
    let tmp = dir.with_extension("fetching");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let tmp_s = path_str(&tmp)?;

    match pin {
        Pin::Git(commit) => {
            run("git", &["init", "-q"], &tmp, &[])?;
            run(
                "git",
                &["fetch", "-q", "--depth", "1", url, commit],
                &tmp,
                &[],
            )?;
            run("git", &["checkout", "-q", "FETCH_HEAD"], &tmp, &[])?;
        }
        Pin::Archive(sha256) => {
            let archive = tmp.join("download");
            let archive_s = path_str(&archive)?;
            run("curl", &["-fsSL", "-o", &archive_s, url], &tmp, &[])?;
            let bytes = std::fs::read(&archive)?;
            check(url, "sha256", sha256, &hex::encode(Sha256::digest(&bytes)))?;
            if url.ends_with(".zip") {
                // Flatten: the files the manifests name sit at the top level.
                run("unzip", &["-q", "-j", &archive_s, "-d", &tmp_s], &tmp, &[])?;
            } else {
                run(
                    "tar",
                    &["xf", &archive_s, "--strip-components=1", "-C", &tmp_s],
                    &tmp,
                    &[],
                )?;
            }
            std::fs::remove_file(&archive)?;
            std::fs::write(tmp.join(ARCHIVE_MARKER), sha256)?;
        }
    }
    verify_existing(&tmp, pin)?;
    std::fs::rename(&tmp, dir)
        .with_context(|| format!("renaming {} → {}", tmp.display(), dir.display()))
}

/// Check that an already-fetched dir matches its pin.
fn verify_existing(dir: &Path, pin: Pin<'_>) -> Result<()> {
    let what = dir.display().to_string();
    match pin {
        Pin::Git(commit) => {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .with_context(|| format!("running git rev-parse in {what}"))?;
            ensure!(out.status.success(), "{what} is not a git checkout");
            check(
                &what,
                "commit",
                commit,
                String::from_utf8_lossy(&out.stdout).trim(),
            )
        }
        Pin::Archive(sha256) => {
            let marker = std::fs::read_to_string(dir.join(ARCHIVE_MARKER)).unwrap_or_default();
            check(&what, "sha256", sha256, marker.trim())
        }
    }
}

fn check(what: &str, field: &str, expected: &str, actual: &str) -> Result<()> {
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "{what}: {field} is {actual:?}, but the manifest pins {expected} \
         (delete the dir to re-fetch)"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const COMMIT: &str = "840c36d3964833e1dc107fcb9ade9c0ad63076c0";
    const SHA: &str = "8a310d0a16c7a90cacd4c884e70faa51c902afed2a89f63aaa0126ab83558a32";

    #[test]
    fn pin_matches_the_url_kind() {
        let git = "https://example.com/x.git";
        let zip = "https://example.com/x.zip";
        assert_eq!(pin(git, Some(COMMIT), None).unwrap(), Pin::Git(COMMIT));
        assert_eq!(pin(zip, None, Some(SHA)).unwrap(), Pin::Archive(SHA));
        assert!(pin(git, None, None).is_err());
        assert!(pin(git, Some("v1.0"), None).is_err());
        assert!(pin(git, Some(COMMIT), Some(SHA)).is_err());
        assert!(pin(zip, None, None).is_err());
        assert!(pin(zip, Some(COMMIT), Some(SHA)).is_err());
    }

    /// An existing archive dir from another pin fails instead of being reused.
    #[test]
    fn existing_archive_dir_with_another_sha_fails() {
        let dir = std::env::temp_dir().join(format!("eryx-build-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(ARCHIVE_MARKER), SHA).unwrap();
        assert!(verify_existing(&dir, Pin::Archive(SHA)).is_ok());
        let other = "0".repeat(64);
        let err = verify_existing(&dir, Pin::Archive(&other)).unwrap_err();
        assert!(err.to_string().contains("delete the dir"), "{err}");
        std::fs::remove_file(dir.join(ARCHIVE_MARKER)).unwrap();
        assert!(verify_existing(&dir, Pin::Archive(SHA)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A git checkout at another commit fails.
    #[test]
    fn existing_git_dir_at_another_commit_fails() {
        let dir = std::env::temp_dir().join(format!("eryx-build-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
                .current_dir(&dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "x"]);
        let err = verify_existing(&dir, Pin::Git(COMMIT)).unwrap_err();
        assert!(err.to_string().contains("commit is"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
