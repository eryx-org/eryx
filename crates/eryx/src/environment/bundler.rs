//! Running the bundler, and installing what it produced (the `env-pack`
//! feature).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use super::{EnvironmentError, MANIFEST_FILE};
use crate::process::{IsolatedLimits, Mount};

pub(super) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> EnvironmentError {
    let context = context.into();
    move |source| EnvironmentError::Io { context, source }
}

/// The directory a JavaScript environment's modules are in.
pub(super) const MODULES_DIR: &str = "modules";

/// Whether `dir` is an existing, empty directory.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Where to build `out`: inside it if it's an existing empty directory (a
/// volume mounted there, say), so the result can be moved in on the same
/// filesystem; otherwise next to it, to be renamed into place. An `out`
/// that holds anything other than an environment is refused up front.
pub(super) fn staging_dir(out: &Path) -> Result<tempfile::TempDir, EnvironmentError> {
    let staging = |dir: &Path| {
        tempfile::Builder::new()
            .prefix(".eryx-staging-")
            .tempdir_in(dir)
            .map_err(io(format!("creating a directory in {}", dir.display())))
    };
    if is_empty_dir(out) {
        return staging(out);
    }
    if out.exists() && !out.join(MANIFEST_FILE).is_file() {
        return Err(EnvironmentError::Build(format!(
            "{} exists and isn't an environment; not replacing it",
            out.display()
        )));
    }
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io(format!("creating {}", parent.display())))?;
    staging(parent)
}

/// Put the environment built in `staging` at `out`.
///
/// - `out` was empty (`staging` is inside it): move the contents up, the
///   manifest last, so it never describes files that aren't there yet.
/// - `out` doesn't exist: rename `staging` to it.
/// - `out` holds an environment: move it aside, rename `staging` in, and
///   delete it, so there's never a half-deleted environment at `out` (only,
///   briefly, none).
///
/// `out` must not be a mount point holding an environment, since that can't
/// be renamed: mount its parent instead, or an empty directory.
pub(super) fn install_environment(
    staging: tempfile::TempDir,
    out: &Path,
) -> Result<(), EnvironmentError> {
    if staging.path().parent() == Some(out) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(staging.path())
            .map_err(io("reading the staging directory"))?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<Result<_, _>>()
            .map_err(io("reading the staging directory"))?;
        entries.sort_by_key(|p| p.ends_with(MANIFEST_FILE));
        for entry in entries {
            let name = entry.file_name().unwrap_or_default();
            std::fs::rename(&entry, out.join(name))
                .map_err(io(format!("moving the build into {}", out.display())))?;
        }
        return Ok(());
    }

    let staged = staging.keep();
    let discard = |staged: &Path| {
        let _ = std::fs::remove_dir_all(staged);
    };
    // A few attempts: a concurrent build can replace `out` in between.
    for _ in 0..8 {
        match std::fs::rename(&staged, out) {
            Ok(()) => return Ok(()),
            Err(e) if !out.exists() => {
                if !staged.exists() {
                    return Err(io(format!("installing {}", out.display()))(e));
                }
                continue;
            }
            Err(_) => {}
        }
        if !out.join(MANIFEST_FILE).is_file() {
            discard(&staged);
            return Err(EnvironmentError::Build(format!(
                "{} exists and isn't an environment; not replacing it",
                out.display()
            )));
        }
        let aside = aside_path(out);
        match std::fs::rename(out, &aside) {
            Ok(()) => {}
            // Another build moved it first; try again.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                discard(&staged);
                return Err(EnvironmentError::Build(format!(
                    "can't replace the environment at {}: {e}. If it's a mount point, \
                     mount its parent (or an empty directory) instead",
                    out.display()
                )));
            }
        }
        let swapped = std::fs::rename(&staged, out);
        if swapped.is_err() {
            // Put the old one back unless another build already replaced it.
            let _ = std::fs::rename(&aside, out);
        } else {
            let _ = std::fs::remove_dir_all(&aside);
        }
        match swapped {
            Ok(()) => return Ok(()),
            Err(_) if out.exists() => {}
            Err(e) => {
                discard(&staged);
                return Err(io(format!("installing {}", out.display()))(e));
            }
        }
    }
    discard(&staged);
    Err(EnvironmentError::Build(format!(
        "{} kept changing while installing; is another build replacing it?",
        out.display()
    )))
}

/// A unique name next to `out` to move the environment there aside to.
fn aside_path(out: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    out.with_file_name(format!(
        ".{name}.old-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// `bundle.json`, as the bundler writes it.
#[derive(serde::Deserialize)]
pub(super) struct BundleReport {
    pub(super) bundler: String,
    pub(super) entries: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) warnings: Vec<String>,
}

/// Bundle `entries` into `out`. `sources` are mounted read-only at their
/// guest paths (under `/src`), and `cwd` is where the tree is among them.
pub(super) async fn run_bundler(
    sources: &[(PathBuf, String)],
    cwd: &str,
    out: &Path,
    entries: &[String],
) -> Result<BundleReport, EnvironmentError> {
    let mut argv: Vec<String> = ["eryx-bundler", "--cwd", cwd, "--out", "/out"]
        .iter()
        .map(ToString::to_string)
        .collect();
    for entry in entries {
        argv.push("--entry".to_string());
        argv.push(entry.clone());
    }
    let mut mounts: Vec<Mount<'_>> = sources
        .iter()
        .map(|(host, guest)| Mount {
            host,
            guest,
            writable: false,
        })
        .collect();
    mounts.push(Mount {
        host: out,
        guest: "/out",
        writable: true,
    });
    let limits = IsolatedLimits {
        memory: 1024 * 1024 * 1024,
        fuel: 500_000_000_000,
        timeout: Duration::from_secs(5 * 60),
        output: 1024 * 1024,
    };
    let (code, _stdout, stderr) = bundler()?
        .run_isolated(&argv, &mounts, &limits)
        .await
        .map_err(|e| EnvironmentError::Build(format!("bundling: {e}")))?;
    if code != 0 {
        return Err(EnvironmentError::Build(format!(
            "bundling failed:\n{}",
            String::from_utf8_lossy(&stderr).trim_end()
        )));
    }
    let report = std::fs::read(out.join("bundle.json")).map_err(io("reading bundle.json"))?;
    serde_json::from_slice(&report)
        .map_err(|e| EnvironmentError::Build(format!("the bundler's bundle.json: {e}")))
}

/// The bundler, loaded on first use: the embedded one, or, if this eryx was
/// built without it (a placeholder is embedded), the one `eryx-precompile
/// setup --bundler` cached. Only a loaded bundler is kept: after a failure,
/// the next pack looks again, so running `setup` fixes a running process.
fn bundler() -> Result<crate::Command, EnvironmentError> {
    static BUNDLER: OnceLock<crate::Command> = OnceLock::new();
    let command = match BUNDLER.get() {
        Some(loaded) => loaded,
        None => {
            let loaded = load_bundler(
                include_bytes!(concat!(env!("OUT_DIR"), "/eryx-bundler.cwasm")),
                crate::cache::setup_bundler,
            )
            .map_err(|e| EnvironmentError::Build(format!("the bundler: {e}")))?;
            // Another build may have loaded it meanwhile; either will do.
            BUNDLER.get_or_init(|| loaded)
        }
    };
    Ok(command.clone())
}

/// Load `embedded`, or what `cached` finds if it's empty.
fn load_bundler(
    embedded: &[u8],
    cached: impl FnOnce() -> Option<(std::path::PathBuf, Result<Vec<u8>, String>)>,
) -> Result<crate::Command, String> {
    let setup = "run `eryx-precompile setup --bundler` (or, in the eryx workspace, \
                 `mise run precompile-bundler`)";
    if !embedded.is_empty() {
        // SAFETY: the build script embeds a bundler that eryx's precompile
        // produced, as it does the runtimes.
        #[allow(unsafe_code)]
        return unsafe { crate::Command::from_precompiled_isolated(embedded) }
            .map_err(|e| format!("the embedded bundler: {e}"));
    }
    let (path, bytes) = match cached() {
        None => {
            return Err(format!(
                "this eryx was built without the bundler, and none is cached: {setup}"
            ));
        }
        // `setup` rebuilds an entry that fails these checks.
        Some((_, Err(reason))) => {
            return Err(format!(
                "not using the cached bundler: {reason}; {setup} again"
            ));
        }
        Some((path, Ok(bytes))) => (path, bytes),
    };
    // SAFETY: `setup` precompiled it with this eryx's precompile, and it's
    // checked against the checksum `setup` wrote, in a directory only this
    // user can write, as eryx's build script checks a cached runtime before
    // embedding it. Its name carries this eryx's version, and one another
    // eryx precompiled fails to load.
    #[allow(unsafe_code)]
    unsafe { crate::Command::from_precompiled_isolated(&bytes) }
        // `setup` would keep an entry that matches its checksum, so it has
        // to go first.
        .map_err(|e| {
            format!(
                "the cached bundler {} doesn't load: {e}; delete it and {}, then {setup} again",
                path.display(),
                crate::cache::setup_checksum_path(&path).display()
            )
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The embedded bundler wins; without one, the cached one is used if it
    /// checked out, and otherwise the error says to run `setup --bundler`.
    #[test]
    fn the_bundler_falls_back_to_the_setup_cache() {
        let embedded: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/eryx-bundler.cwasm"));
        if !embedded.is_empty() {
            assert!(load_bundler(embedded, || panic!("the cache was consulted")).is_ok());
            // Without an embedded one, the cached bytes load.
            assert!(load_bundler(b"", || Some(("b.cwasm".into(), Ok(embedded.to_vec())))).is_ok());
        }

        let err = load_bundler(b"", || None).unwrap_err();
        assert!(
            err.contains("built without the bundler")
                && err.contains("eryx-precompile setup --bundler"),
            "{err}"
        );
        let err = load_bundler(b"", || {
            Some(("b.cwasm".into(), Err("it doesn't match".into())))
        })
        .unwrap_err();
        assert!(
            err.contains("not using the cached bundler: it doesn't match")
                && err.contains("setup --bundler"),
            "{err}"
        );
        let err = load_bundler(b"", || {
            Some(("b.cwasm".into(), Ok(b"not a cwasm".to_vec())))
        })
        .unwrap_err();
        assert!(
            err.starts_with("the cached bundler b.cwasm doesn't load: ")
                && err.contains("delete it and b.cwasm.sha256"),
            "{err}"
        );
    }

    #[test]
    fn refuses_to_replace_a_non_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("precious"), "x").unwrap();
        assert!(staging_dir(&out).is_err());
        assert!(out.join("precious").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn stages_inside_an_empty_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("volume");
        std::fs::create_dir(&out).unwrap();
        let staging = staging_dir(&out).unwrap();
        assert_eq!(staging.path().parent(), Some(out.as_path()));
        std::fs::write(staging.path().join(MANIFEST_FILE), "{}").unwrap();
        std::fs::create_dir(staging.path().join("modules")).unwrap();
        install_environment(staging, &out).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["eryx-environment.json", "modules"]);
    }
}
