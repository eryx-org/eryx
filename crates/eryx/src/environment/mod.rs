//! Environments: package sets built ahead of time and mounted by sandboxes.
//!
//! An environment is a directory that sandboxes mount read-only. Sandboxes
//! never see package names or a registry. eryx doesn't resolve or fetch
//! packages: something else installs them (`npm ci --ignore-scripts`, pnpm,
//! or the eryx-run service, which resolves and fetches them from a
//! definition), and [`pack`] turns the installed tree into an environment,
//! with the `env-pack` feature:
//!
//! ```text
//! eryx-precompile env pack . --name data-tools --out envs/data-tools
//! ```
//!
//! Loading and mounting one ([`Environment::load`],
//! `SandboxBuilder::with_environment`) only needs `environment`, without the
//! bundler.
//!
//! - **JavaScript:** [`pack`] bundles the installed packages into ES modules
//!   with rolldown. The bundler runs as a WASI command inside wasmtime,
//!   seeing only a mirror of the packages and its output directory, with no
//!   network, so the untrusted package source is parsed in a sandbox. The
//!   environment's `exports` map each importable specifier to its module.
//! - **Python** and the **shell**: not supported yet
//!   ([`EnvironmentError::Unsupported`]).
//!
//! The directory format is [`Manifest`]'s (book: Environment Format). The
//! `env-pack` feature embeds the bundler if it's there at build time;
//! otherwise (e.g. a crates.io build) it's loaded from the cache
//! `eryx-precompile setup --bundler` writes ([`crate::cache::setup_bundler`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod manifest;
pub use manifest::{
    Builder, Content, ContentHash, Inputs, Language, MANIFEST_FILE, MANIFEST_FORMAT,
    MANIFEST_VERSION, Manifest, content_hash,
};

use crate::Language as GuestLanguage;

#[cfg(feature = "env-pack")]
mod bundler;
#[cfg(feature = "env-pack")]
mod pack;
#[cfg(feature = "env-pack")]
pub use pack::{PackOptions, pack};

/// Where a sandbox sees the environment it was given with
/// `SandboxBuilder::with_environment`, read-only.
pub const MOUNT_PATH: &str = "/eryx/environment";

/// Why packing or loading an environment failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvironmentError {
    /// Environments for this language can't be packed or loaded yet.
    #[error("{0} environments aren't supported yet")]
    Unsupported(Language),
    /// The manifest is invalid, or describes something this eryx can't
    /// load (another format version, paths outside the environment).
    #[error("environment: {0}")]
    Manifest(String),
    /// The environment's files or manifest don't match its hash, or it's
    /// another environment than the one expected.
    #[error("{what} doesn't match its integrity hash {expected}")]
    Integrity {
        /// What was checked.
        what: String,
        /// The hash it should have.
        expected: String,
    },
    /// The language's pack step (e.g. the bundler) failed.
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
        let language = guest_language(manifest.language);
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
            Err(EnvironmentError::Integrity {
                what: format!("the environment at {}", dir.as_ref().display()),
                expected: hash.to_string(),
            })
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

pub(crate) fn guest_language(language: Language) -> GuestLanguage {
    match language {
        Language::JavaScript => GuestLanguage::JavaScript,
        Language::Python => GuestLanguage::Python,
        Language::Shell => GuestLanguage::Shell,
    }
}
