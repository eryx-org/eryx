//! Environments: package sets built ahead of time and mounted by sandboxes.
//!
//! An environment is built once, from an [`EnvironmentDefinition`] (a TOML
//! file naming the language and its packages, or one built in code with
//! [`EnvironmentDefinition::new`]), into a directory that
//! sandboxes mount read-only. Sandboxes never see package names or a
//! registry, and building needs the network only to fetch packages, so it
//! belongs in CI or a deploy step:
//!
//! ```text
//! eryx-precompile env build data-tools.toml --out envs/data-tools
//! ```
//!
//! Building one is `environment::build`, or `environment::pack` from packages
//! already installed (e.g. by `npm ci`), with the `packages` feature; loading and
//! mounting one ([`Environment::load`], `SandboxBuilder::with_environment`)
//! only needs `environment`, without the bundler or an HTTP client.
//!
//! What a build does depends on the language:
//!
//! - **JavaScript** (`[npm]`): resolves and installs the packages (see
//!   `eryx_packages::npm`), writing or reusing the lockfile, then bundles
//!   them into ES modules with rolldown. The bundler runs as a WASI command
//!   inside wasmtime, with only the installed packages and its output
//!   directory mounted and no network, so the untrusted package source is
//!   parsed in a sandbox. The environment's `exports` map each importable
//!   specifier to its module.
//! - **Python** and the **shell**: not supported yet
//!   ([`EnvironmentError::Unsupported`]).
//!
//! The `packages` feature embeds the bundler if it's there at build time;
//! otherwise (e.g. a crates.io build) it's loaded from the cache
//! `eryx-precompile setup --bundler` writes ([`crate::cache::setup_bundler`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use eryx_packages::environment::{
    Builder, Content, EnvironmentDefinition, Inputs, Language, LimitsSection, Manifest, NpmSection,
    Size,
};

use crate::Language as GuestLanguage;

#[cfg(feature = "packages")]
mod build;
#[cfg(feature = "packages")]
pub use build::{BuildOptions, build};
#[cfg(feature = "packages")]
mod pack;
#[cfg(feature = "packages")]
pub use pack::{PackOptions, pack};

/// Where a sandbox sees the environment it was given with
/// `SandboxBuilder::with_environment`, read-only.
pub const MOUNT_PATH: &str = "/eryx/environment";

/// Why building or loading an environment failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvironmentError {
    /// Environments for this language can't be built yet.
    #[error("{0} environments aren't supported yet")]
    Unsupported(Language),
    /// Resolving, fetching or installing the packages failed, or the
    /// definition or manifest is invalid.
    #[error(transparent)]
    Packages(#[from] eryx_packages::Error),
    /// The language's build step (e.g. the bundler) failed.
    #[error("{0}")]
    Build(String),
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// A built environment: its directory and manifest.
#[derive(Debug, Clone)]
pub struct Environment {
    root: PathBuf,
    manifest: Manifest,
    language: GuestLanguage,
}

impl Environment {
    /// Load the environment in `dir`, checking its manifest, that its
    /// exports exist, and that it still hashes to what the manifest records
    /// (see [`Manifest::verify`] for what that does and doesn't guarantee).
    ///
    /// # Errors
    ///
    /// Fails if there's no valid manifest, the environment was changed, or
    /// it's for a language this eryx doesn't know.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, EnvironmentError> {
        let root = dir.as_ref().to_path_buf();
        let manifest = Manifest::read(&root)?;
        manifest.verify(&root)?;
        let language = guest_language(manifest.language)?;
        Ok(Self {
            root,
            manifest,
            language,
        })
    }

    /// [`load`](Self::load), and check it's the environment whose
    /// [`Content::hash`] is `hash`: pin one built in CI, so a sandbox can't
    /// be handed another environment at the same path.
    ///
    /// # Errors
    ///
    /// As for [`load`](Self::load), or if it's another environment.
    pub fn load_expecting(dir: impl AsRef<Path>, hash: &str) -> Result<Self, EnvironmentError> {
        let environment = Self::load(&dir)?;
        if environment.manifest.content.hash == hash {
            Ok(environment)
        } else {
            Err(eryx_packages::Error::Integrity {
                what: format!("the environment at {}", dir.as_ref().display()),
                expected: hash.to_string(),
            }
            .into())
        }
    }

    /// The environment's directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Its manifest.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The guest language it's for.
    #[must_use]
    pub fn language(&self) -> GuestLanguage {
        self.language
    }

    /// What it makes importable, by name, as paths inside [`root`](Self::root).
    #[must_use]
    pub fn exports(&self) -> &BTreeMap<String, String> {
        &self.manifest.exports
    }
}

pub(crate) fn guest_language(language: Language) -> Result<GuestLanguage, EnvironmentError> {
    match language {
        Language::JavaScript => Ok(GuestLanguage::JavaScript),
        Language::Python => Ok(GuestLanguage::Python),
        Language::Shell => Ok(GuestLanguage::Shell),
        other => Err(EnvironmentError::Unsupported(other)),
    }
}
