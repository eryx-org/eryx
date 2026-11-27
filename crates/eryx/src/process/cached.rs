//! Loading a command file that was fetched elsewhere (`wkg oci pull`, oras, a
//! volume), checked against a pinned digest, and compiled once into eryx's
//! cache (#998).

use std::path::Path;

use sha2::{Digest, Sha256};

use super::Command;
use crate::error::{Error, ErrorKind};

impl Command {
    /// Compile a command from a `.wasm` file whose sha256 must be `sha256`
    /// (`sha256:<hex>`), as [`from_file`](Self::from_file) does otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CommandDigest`] if the file's digest isn't
    /// `sha256` (or `sha256` isn't a sha256 digest), and the errors of
    /// [`from_bytes`](Self::from_bytes).
    pub fn from_file_expecting(path: impl AsRef<Path>, sha256: &str) -> Result<Self, Error> {
        let wasm = read(path.as_ref())?;
        check_digest(&wasm, sha256)?;
        Self::from_bytes(&wasm)
    }

    /// Load a command from a `.wasm` file, compiling it once: the compiled
    /// command is cached under `commands/` in eryx's cache directory, keyed by
    /// the file's sha256, the eryx version and the engine's compatibility hash
    /// (target, CPU features and settings), so a later load (even of a copy
    /// elsewhere) skips compiling. With `expected` (`sha256:<hex>`), the file
    /// must have that digest. A command that imports what children don't get
    /// is refused before it's compiled (see [`inspect`](Self::inspect)).
    ///
    /// The cache only saves compiling: if its directory can't be created, isn't
    /// private to this user, or can't be written (a read-only filesystem, say),
    /// the command is compiled in memory and a warning is logged. Builds
    /// without the `embedded`, `preinit` or `env-pack` feature can't load
    /// precompiled code, so they compile it on every load.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CommandDigest`] on a digest mismatch,
    /// [`ErrorKind::CommandImports`] if it can't run as a command, and an
    /// error if it can't be read or compiled.
    pub fn load_cached(path: impl AsRef<Path>, expected: Option<&str>) -> Result<Self, Error> {
        #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
        let dir = match crate::cache::default_cache_root() {
            Ok(root) => Some(root.join("commands")),
            Err(e) => {
                uncached(Path::new("(eryx cache directory)"), &e);
                None
            }
        };
        #[cfg(not(any(feature = "embedded", feature = "preinit", feature = "env-pack")))]
        let dir: Option<std::path::PathBuf> = None;
        load_cached_in(path.as_ref(), expected, dir.as_deref())
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| {
        ErrorKind::Initialization(format!("failed to read command {}: {e}", path.display())).into()
    })
}

/// The file's digest as `sha256:<hex>`.
fn digest(wasm: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(wasm)))
}

fn check_digest(wasm: &[u8], expected: &str) -> Result<(), Error> {
    let well_formed = expected
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    let actual = if well_formed {
        digest(wasm)
    } else {
        format!("(not a sha256:<64 hex> digest: {expected})")
    };
    if well_formed && actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(ErrorKind::CommandDigest {
        expected: expected.to_owned(),
        actual,
    }
    .into())
}

/// [`Command::load_cached`], caching in `dir` (`None`: no cache).
fn load_cached_in(
    path: &Path,
    expected: Option<&str>,
    dir: Option<&Path>,
) -> Result<Command, Error> {
    let wasm = read(path)?;
    if let Some(expected) = expected {
        check_digest(&wasm, expected)?;
    }
    // Refused the way loading refuses it (`imports::explain`), but before
    // anything is compiled.
    let info = Command::inspect(&wasm)?;
    if let Some(problem) = info.problem {
        let missing: Vec<_> = info
            .imports
            .into_iter()
            .filter(|i| matches!(i.support, crate::ImportSupport::Missing(_)))
            .collect();
        return Err(if missing.is_empty() {
            ErrorKind::Initialization(problem)
        } else {
            ErrorKind::CommandImports { missing }
        }
        .into());
    }
    match dir {
        Some(dir) => compiled(&wasm, dir),
        None => Command::from_bytes(&wasm),
    }
}

/// Load `wasm` from its compiled copy in `dir`, compiling and caching it
/// first if there's none (or it doesn't load).
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
#[allow(unsafe_code)]
fn compiled(wasm: &[u8], dir: &Path) -> Result<Command, Error> {
    use std::hash::{Hash, Hasher};
    use std::io::Write;

    let dir = match crate::cache::ensure_private_dir(dir) {
        Ok(dir) => dir,
        Err(e) => {
            uncached(dir, &e);
            return Command::from_bytes(wasm);
        }
    };
    let mut engine = std::collections::hash_map::DefaultHasher::new();
    crate::Executor::child_engine()?
        .precompile_compatibility_hash()
        .hash(&mut engine);
    // Keyed by the bytes being loaded, never by where they came from: a
    // different file at the same path must not reuse another's compiled
    // code, and only the private cache directory is trusted.
    let cwasm = dir.join(format!(
        "{}-{}-{:016x}.cwasm",
        digest(wasm).replace(':', "-"),
        env!("CARGO_PKG_VERSION"),
        engine.finish()
    ));
    // SAFETY: the directory is checked to be private to this user, and
    // entries are only ever replaced by rename, so a mapped file never
    // changes.
    if let Ok(command) = unsafe { Command::from_precompiled_file(&cwasm) } {
        return Ok(command);
    }
    // A WASI preview 1 module compiles as the command component it's adapted
    // into, or its .cwasm wouldn't load as a command.
    let component = Command::to_component(wasm)?;
    let precompiled = crate::Executor::precompile(&component)?;
    // Through a temporary file and a rename, so a concurrent load of the same
    // command only ever sees a complete file.
    let written = tempfile::NamedTempFile::new_in(&dir).and_then(|mut file| {
        file.write_all(&precompiled)?;
        file.persist(&cwasm).map_err(|e| e.error)
    });
    if let Err(e) = written {
        uncached(&dir, &e);
        // SAFETY: this process compiled these bytes just now.
        return unsafe { Command::from_precompiled(&precompiled) };
    }
    drop(precompiled);
    release_freed_memory();
    // SAFETY: as above; this process just wrote it.
    unsafe { Command::from_precompiled_file(&cwasm) }
}

/// Hand the memory compiling freed back to the OS. Compiling a big command
/// (gcx, 70 MB of wasm) frees over a gigabyte, which glibc's allocator
/// otherwise keeps for the life of the process.
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
#[allow(unsafe_code)]
fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free heap memory.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Warn, once per load, that a command is compiled without the cache.
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
fn uncached(dir: &Path, reason: &dyn std::fmt::Display) {
    tracing::warn!(
        dir = %dir.display(),
        %reason,
        "can't use the command cache; compiling the command in memory"
    );
}

#[cfg(not(any(feature = "embedded", feature = "preinit", feature = "env-pack")))]
fn compiled(wasm: &[u8], _dir: &Path) -> Result<Command, Error> {
    Command::from_bytes(wasm)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::process::tests::test_cmd_wasm;

    #[test]
    fn pins_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-cmd.wasm");
        let wasm = test_cmd_wasm();
        std::fs::write(&path, &wasm).unwrap();
        let pin = digest(&wasm);

        Command::from_file_expecting(&path, &pin).unwrap();
        Command::from_file_expecting(&path, &pin.to_uppercase().replace("SHA256", "sha256"))
            .unwrap();
        let wrong = digest(b"something else");
        for bad in [wrong.as_str(), "sha256:abc", "md5:00"] {
            let err = Command::from_file_expecting(&path, bad).unwrap_err();
            assert!(
                matches!(err.kind(), ErrorKind::CommandDigest { expected, .. } if expected == bad),
                "{err}"
            );
            let err = load_cached_in(&path, Some(bad), None).unwrap_err();
            assert!(
                matches!(err.kind(), ErrorKind::CommandDigest { .. }),
                "{err}"
            );
        }
        load_cached_in(&path, Some(&pin), None).unwrap();
    }

    /// A command importing what children don't get is refused before
    /// anything is compiled or cached.
    #[test]
    fn unrunnable_commands_are_refused_before_compiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.wasm");
        let wasm =
            wat::parse_str(r#"(component (import "wasi:http/outgoing-handler@0.2.0" (instance)))"#)
                .unwrap();
        std::fs::write(&path, wasm).unwrap();
        let cache = dir.path().join("cache");
        let err = load_cached_in(&path, None, Some(&cache)).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::CommandImports { .. }),
            "{err}"
        );
        assert!(!cache.exists());
    }

    /// An unusable cache directory (here a file in its place, or one that
    /// can't be written) only costs the cache: the command is compiled in
    /// memory, and a wrong pin is still refused.
    #[test]
    fn an_unusable_cache_compiles_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-cmd.wasm");
        let wasm = test_cmd_wasm();
        std::fs::write(&path, &wasm).unwrap();
        let not_a_dir = dir.path().join("cache");
        std::fs::write(&not_a_dir, b"").unwrap();
        load_cached_in(&path, Some(&digest(&wasm)), Some(&not_a_dir)).unwrap();
        let err = load_cached_in(&path, Some(&digest(b"other")), Some(&not_a_dir)).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::CommandDigest { .. }),
            "{err}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let read_only = dir.path().join("read-only");
            std::fs::create_dir(&read_only).unwrap();
            std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o500)).unwrap();
            // Root writes anyway, so there's no failure to fall back from.
            if std::fs::write(read_only.join("probe"), b"").is_err() {
                load_cached_in(&path, None, Some(&read_only)).unwrap();
                assert_eq!(std::fs::read_dir(&read_only).unwrap().count(), 0);
            }
        }
    }

    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    fn cwasms(cache: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".cwasm"))
            .collect();
        names.sort();
        names
    }

    /// The compiled command is cached in the cache directory, keyed by the
    /// file's content, never beside the file: a different file at the same
    /// path gets its own, and a second load reuses it.
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    #[test]
    fn compiled_commands_are_cached_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let path = out.join("rg.wasm");
        let cache = dir.path().join("cache");
        let wasm = test_cmd_wasm();
        // The same command with a custom section appended: other bytes, still
        // a valid component.
        let mut other = wasm.clone();
        other.extend_from_slice(&[0, 3, 2, b'h', b'i']);

        std::fs::write(&path, &wasm).unwrap();
        load_cached_in(&path, None, Some(&cache)).unwrap();
        let first = cwasms(&cache);
        assert_eq!(first.len(), 1);
        let modified = || {
            std::fs::metadata(cache.join(&first[0]))
                .unwrap()
                .modified()
                .unwrap()
        };
        let before = modified();
        load_cached_in(&path, None, Some(&cache)).unwrap();
        assert_eq!(modified(), before);

        std::fs::write(&path, &other).unwrap();
        load_cached_in(&path, None, Some(&cache)).unwrap();
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        let mut expected: Vec<_> = [&wasm, &other]
            .iter()
            .map(|w| digest(w).replace(':', "-"))
            .collect();
        expected.sort();
        let names = cwasms(&cache);
        assert_eq!(names.len(), 2, "{names:?}");
        for (name, prefix) in names.iter().zip(&expected) {
            assert!(name.starts_with(prefix.as_str()), "{name} vs {prefix}");
        }
    }

    /// A WASI preview 1 command module is compiled as its adapted component,
    /// so its cached `.cwasm` loads as a command, the second time too.
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    #[test]
    fn preview1_modules_are_compiled_as_components() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-cmd.wasm");
        std::fs::write(
            &path,
            crate::process::tests::test_cmd_wasm_for("wasm32-wasip1"),
        )
        .unwrap();
        let cache = dir.path().join("cache");
        load_cached_in(&path, None, Some(&cache)).unwrap();
        load_cached_in(&path, None, Some(&cache)).unwrap();
        assert_eq!(cwasms(&cache).len(), 1);
    }
}
