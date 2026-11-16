//! Resolve, fetch and cache packages for eryx sandboxes.
//!
//! Everything here runs on the host, ahead of the sandbox: the sandbox itself
//! never talks to a package registry. Nothing from a package is ever executed
//! here either (no install scripts); packages are only downloaded, checked
//! against their integrity hashes, and unpacked.
//!
//! - [`environment`] has the types of environments: the hand-written
//!   definition (TOML) and the manifest of a built one.
//! - [`npm`] resolves npm packages to a [`npm::Lockfile`] and installs them as
//!   a `node_modules` tree, which a bundler turns into the modules a
//!   JavaScript sandbox imports.
//! - [`fetch`], [`integrity`], [`blobs`] and [`extract`] are the parts any
//!   registry needs (size-capped downloads, SRI hashes, a content-addressed
//!   cache, safe archive extraction), so a Python package index can use them
//!   too.

pub mod blobs;
pub mod environment;
pub mod extract;
#[cfg(feature = "npm")]
pub mod fetch;
pub mod integrity;
#[cfg(feature = "npm")]
pub mod npm;

/// Why resolving, fetching or installing packages failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A request failed, or its response was refused (status, size, host).
    #[error("fetching {url}: {message}")]
    Fetch {
        /// What was being fetched.
        url: String,
        /// Why it failed.
        message: String,
    },
    /// Downloaded or cached bytes didn't match their integrity hash.
    #[error("{what} doesn't match its integrity hash {expected}")]
    Integrity {
        /// What was checked, e.g. a tarball URL.
        what: String,
        /// The SRI string it should match.
        expected: String,
    },
    /// A package archive was malformed or unsafe to unpack.
    #[error("unpacking {what}: {message}")]
    Extract {
        /// The archive.
        what: String,
        /// Why it was refused.
        message: String,
    },
    /// No version of a package satisfies what was asked for.
    #[error("resolving {spec}: {message}")]
    Resolve {
        /// The requested package, e.g. `zod@^3`.
        spec: String,
        /// Why it can't be resolved.
        message: String,
    },
    /// A configured limit was exceeded.
    #[error("limit exceeded: {0}")]
    Limit(String),
    /// Offline mode needed something that isn't cached.
    #[error("offline, and {0} isn't cached")]
    Offline(String),
    /// A lockfile couldn't be read or written.
    #[error("lockfile: {0}")]
    Lockfile(String),
    /// An environment definition or manifest is invalid.
    #[error("environment: {0}")]
    Definition(String),
    /// A filesystem operation on the cache or an install failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

/// A `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
