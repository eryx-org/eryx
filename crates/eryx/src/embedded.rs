//! Embedded resources for zero-configuration sandboxes.
//!
//! This module provides automatic extraction and caching of embedded resources:
//!
//! - **Python stdlib**: Compressed stdlib extracted to the cache directory on first use
//! - **Pre-compiled runtimes**: Written to disk for mmap-based loading (10x less
//!   memory). Python's is always embedded; JavaScript's and the shell's with
//!   the `javascript` and `shell` features.
//! - **Default shell commands**: with the `coreutils` feature (which `shell`
//!   turns on), uutils coreutils, registered by `Sandbox::embedded().shell()`
//!   under each of [`COREUTILS_TOOLS`].
//!
//! Both go into a per-user cache directory: `$XDG_CACHE_HOME/eryx`, else the
//! platform cache directory (`~/.cache/eryx`, `~/Library/Caches/eryx`,
//! `%LOCALAPPDATA%\eryx`), else `$TMPDIR/eryx-<uid>`. On Unix the directory is
//! created with mode `0700` and must be owned by the current user and not be
//! writable by others. An extracted runtime is compared against the embedded
//! bytes before it is used, and rewritten if it differs.
//!
//! # Features
//!
//! This module is only available when the `embedded` feature is enabled.
//! For stdlib-only embedding, see [`crate::embedded_stdlib`].
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::embedded::EmbeddedResources;
//!
//! // Get paths to embedded resources (extracts on first call)
//! let resources = EmbeddedResources::get()?;
//!
//! let sandbox = Sandbox::builder()
//!     .with_precompiled_file(&resources.runtime_path)
//!     .with_python_stdlib(&resources.stdlib_path)
//!     .build()?;
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::embedded_stdlib::EmbeddedStdlib;
use crate::error::{Error, ErrorKind};
use crate::language::Language;

/// An embedded pre-compiled runtime.
struct EmbeddedRuntime {
    language: Language,
    bytes: &'static [u8],
    /// Short hash of `bytes` for naming its cached copy: the first 16 hex
    /// characters of SHA-256, computed by the build script.
    content_hash: &'static str,
}

const PYTHON_RUNTIME: EmbeddedRuntime = EmbeddedRuntime {
    language: Language::Python,
    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/runtime-python.cwasm")),
    content_hash: env!("ERYX_EMBEDDED_RUNTIME_HASH"),
};

#[cfg(feature = "javascript")]
const JAVASCRIPT_RUNTIME: EmbeddedRuntime = EmbeddedRuntime {
    language: Language::JavaScript,
    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/runtime-javascript.cwasm")),
    content_hash: env!("ERYX_EMBEDDED_RUNTIME_HASH_JAVASCRIPT"),
};

#[cfg(feature = "shell")]
const SHELL_RUNTIME: EmbeddedRuntime = EmbeddedRuntime {
    language: Language::Shell,
    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/runtime-shell.cwasm")),
    content_hash: env!("ERYX_EMBEDDED_RUNTIME_HASH_SHELL"),
};

/// The uutils coreutils tools the embedded shell runs as external commands,
/// with the `coreutils` feature, and the names to register a `coreutils.cwasm`
/// from `eryx-precompile setup --language shell` under. Must match the
/// `--features` in `clis/coreutils.toml`, which the multicall binary is built
/// from.
pub const COREUTILS_TOOLS: [&str; 32] = [
    "cat",
    "head",
    "tail",
    "ls",
    "wc",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "touch",
    "sort",
    "seq",
    "cut",
    "tr",
    "uniq",
    "basename",
    "dirname",
    "tee",
    "sleep",
    "paste",
    "comm",
    "date",
    "base64",
    "realpath",
    "readlink",
    "rmdir",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "b2sum",
    "printenv",
];

/// The embedded coreutils multicall command, loaded on first use.
#[cfg(feature = "coreutils")]
pub(crate) fn coreutils() -> Result<crate::Command, Error> {
    static COREUTILS: OnceLock<Result<crate::Command, String>> = OnceLock::new();
    COREUTILS
        .get_or_init(|| {
            let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/coreutils.cwasm"));
            // SAFETY: the build script embeds a coreutils that eryx's precompile
            // produced, as it does the runtimes.
            #[allow(unsafe_code)]
            unsafe { crate::Command::from_precompiled(bytes) }.map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| ErrorKind::Initialization(format!("embedded coreutils: {e}")).into())
}

/// The embedded shell as a command, `sh`/`bash` for children that run
/// them (see [`crate::Command::guest`]), loaded once per process.
#[cfg(feature = "shell")]
pub(crate) fn shell_command() -> Result<crate::Command, Error> {
    static SHELL: OnceLock<Result<crate::Command, String>> = OnceLock::new();
    SHELL
        .get_or_init(|| crate::Command::guest(Language::Shell).map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| ErrorKind::Initialization(format!("embedded shell command: {e}")).into())
}

/// The runtime embedded for `language`, if this build has one.
fn embedded_runtime(language: Language) -> Option<&'static EmbeddedRuntime> {
    match language {
        Language::Python => Some(&PYTHON_RUNTIME),
        #[cfg(feature = "javascript")]
        Language::JavaScript => Some(&JAVASCRIPT_RUNTIME),
        #[cfg(not(feature = "javascript"))]
        Language::JavaScript => None,
        #[cfg(feature = "shell")]
        Language::Shell => Some(&SHELL_RUNTIME),
        #[cfg(not(feature = "shell"))]
        Language::Shell => None,
    }
}

/// Whether this build embeds a runtime for `language`.
#[must_use]
pub fn has_runtime(language: Language) -> bool {
    embedded_runtime(language).is_some()
}

/// The path of `language`'s embedded runtime, extracted to the cache
/// directory on first call, or `None` if this build doesn't embed one.
///
/// # Errors
///
/// Returns an error if extraction fails.
pub fn runtime_path(language: Language) -> Result<Option<&'static Path>, Error> {
    static PATHS: [OnceLock<Result<PathBuf, String>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];

    let Some(runtime) = embedded_runtime(language) else {
        return Ok(None);
    };
    PATHS[usize::from(language.snapshot_tag())]
        .get_or_init(|| {
            let temp_dir = crate::cache::default_cache_root().map_err(|e| e.to_string())?;
            runtime.extract(temp_dir).map_err(|e| e.to_string())
        })
        .as_ref()
        .map(|path| Some(path.as_path()))
        .map_err(|e| ErrorKind::Initialization(e.clone()).into())
}

/// Paths to extracted embedded resources.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EmbeddedResources {
    /// Path to the extracted Python standard library directory.
    pub stdlib_path: PathBuf,

    /// Path to the pre-compiled runtime file (for mmap loading).
    pub runtime_path: PathBuf,

    /// The cache directory the resources were extracted to.
    #[allow(dead_code)]
    temp_dir: PathBuf,
}

impl EmbeddedResources {
    /// Get the embedded resources, extracting them on first call.
    ///
    /// Resources are extracted to the per-user cache directory described in
    /// the [module docs](self) and reused by later processes. Subsequent calls
    /// return the same paths.
    ///
    /// # Errors
    ///
    /// Returns an error if resource extraction fails.
    pub fn get() -> Result<&'static Self, Error> {
        static RESOURCES: OnceLock<Result<EmbeddedResources, String>> = OnceLock::new();

        RESOURCES
            .get_or_init(|| Self::extract().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| ErrorKind::Initialization(e.clone()).into())
    }

    /// Extract all embedded resources to the cache directory.
    fn extract() -> Result<Self, Error> {
        let temp_base = crate::cache::default_cache_root()?.to_path_buf();

        // Delegate stdlib extraction to EmbeddedStdlib (shared with embedded-stdlib feature)
        let stdlib = EmbeddedStdlib::get()?;
        let stdlib_path = stdlib.path().to_path_buf();
        let runtime_path = runtime_path(Language::Python)?
            .ok_or_else(|| ErrorKind::Initialization("no embedded Python runtime".into()))?
            .to_path_buf();

        Ok(Self {
            stdlib_path,
            runtime_path,
            temp_dir: temp_base,
        })
    }

    /// Get the path to the Python standard library.
    #[must_use]
    pub fn stdlib(&self) -> &Path {
        &self.stdlib_path
    }

    /// Get the path to the pre-compiled runtime.
    #[must_use]
    pub fn runtime(&self) -> &Path {
        &self.runtime_path
    }
}

impl EmbeddedRuntime {
    /// Extract the runtime to the cache directory.
    fn extract(&self, temp_dir: &Path) -> Result<PathBuf, Error> {
        // Include version AND content hash in filename to handle both version upgrades
        // and development rebuilds (where version stays the same but content changes)
        let version = env!("CARGO_PKG_VERSION");
        let language = self.language;
        let content_hash = self.content_hash;
        let runtime_path =
            temp_dir.join(format!("runtime-{language}-{version}-{content_hash}.cwasm"));

        // Reuse a previous extraction only if it's ours and byte-identical:
        // the file is loaded as native code, so the name alone proves nothing.
        if self.matches(&runtime_path) {
            tracing::debug!(path = %runtime_path.display(), "Using cached runtime");
            return Ok(runtime_path);
        }

        // Clean up old runtime files with different hashes (same version, stale content).
        // Never our own: another process may have extracted it since the check
        // above, and removing it breaks that process's loads until we replace it.
        if let Ok(entries) = std::fs::read_dir(temp_dir) {
            let prefix = format!("runtime-{language}-{version}-");
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(&prefix)
                    && name_str.ends_with(".cwasm")
                    && entry.path() != runtime_path
                {
                    tracing::debug!(path = %entry.path().display(), "Removing stale runtime");
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        tracing::info!(path = %runtime_path.display(), "Extracting embedded runtime");
        crate::cache::publish_file(&runtime_path, self.bytes)
            .map_err(|e| ErrorKind::Initialization(format!("Failed to write runtime file: {e}")))?;

        Ok(runtime_path)
    }

    /// Whether `path` is a regular file owned by this user, not writable by
    /// others, and identical to the embedded bytes.
    fn matches(&self, path: &Path) -> bool {
        crate::cache::file_matches(path, self.bytes)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn extracted_runtime_must_match_embedded_bytes() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let first = PYTHON_RUNTIME.extract(temp_dir.path()).unwrap();
        assert!(PYTHON_RUNTIME.matches(&first));

        // Same name, different contents: rewritten rather than reused.
        std::fs::write(&first, b"not the runtime").unwrap();
        let second = PYTHON_RUNTIME.extract(temp_dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(std::fs::read(&second).unwrap() == PYTHON_RUNTIME.bytes);
    }

    /// Processes starting together against an empty cache (#850): once any of
    /// them has the runtime, it must stay loadable while the rest extract.
    #[test]
    fn concurrent_first_use_keeps_runtime_loadable() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        crate::cache::assert_concurrent_publish_stays_loadable(|| {
            PYTHON_RUNTIME.extract(temp_dir.path()).unwrap()
        });
    }

    #[test]
    fn embedded_runtime_is_included() {
        // Just verify the bytes are included
        assert!(
            PYTHON_RUNTIME.bytes.len() > 1_000_000,
            "Embedded runtime should be > 1MB"
        );
    }
}

#[cfg(all(test, feature = "coreutils"))]
#[allow(clippy::unwrap_used)]
mod coreutils_tests {
    /// The tool list matches the features the embedded binary is built with.
    #[test]
    fn coreutils_tools_match_the_manifest() {
        let manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../clis/coreutils.toml"
        ))
        .unwrap();
        let mut lines = manifest.lines().map(str::trim);
        lines.find(|l| *l == "\"--features\",").unwrap();
        let built: Vec<&str> = lines
            .next()
            .unwrap()
            .trim_matches(['"', ','])
            .split(',')
            .collect();
        assert_eq!(built, super::COREUTILS_TOOLS);
    }
}
