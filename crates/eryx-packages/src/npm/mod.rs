//! npm packages: resolve them to a [`Lockfile`] and install the tarballs as a
//! `node_modules` tree.
//!
//! ```no_run
//! # async fn run() -> eryx_packages::Result<()> {
//! use eryx_packages::npm::Install;
//!
//! let installed = Install::new(["zod@^3", "date-fns"], "/var/cache/eryx/packages")
//!     .lockfile("eryx-npm.lock")
//!     .run()
//!     .await?;
//! println!("{}", installed.node_modules.display());
//! # Ok(())
//! # }
//! ```
//!
//! Safety properties, all on by default:
//! - Nothing from a package runs: no install or lifecycle scripts.
//! - Every tarball must match the registry's SHA-512 integrity hash, on
//!   download and again when read from the cache; a lockfile pins them.
//! - Only the registry's host (and hosts added with
//!   [`Registry::allow_tarball_host`]) is contacted, over HTTPS.
//! - Versions published less than [`Install::min_release_age`] ago (default
//!   3 days) aren't installed, so a freshly compromised release isn't picked
//!   up before it can be caught.
//! - Package counts, download sizes and unpacked sizes are capped
//!   ([`Limits`]), and archives can't write outside their package directory.
//! - [`Install::offline`] uses only the lockfile and the cache.
//!
//! The tree holds exactly the lockfile's packages, unpacked in path order:
//! dependencies a tarball bundles under its own `node_modules/`
//! (`bundleDependencies`) are skipped, since they aren't in the lockfile.

mod lockfile;
mod packument;
mod resolve;
mod spec;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use futures::StreamExt as _;

pub use lockfile::{LOCKFILE_VERSION, LockedPackage, Lockfile};
pub use spec::Spec;

use crate::blobs::Blobs;
use crate::extract::{ExtractLimits, unpack_tgz_stripped};
use crate::fetch::{Fetch, HttpFetch};
use crate::integrity::Integrity;
use crate::{Error, Result};

/// Tarballs downloaded at once.
const CONCURRENT_DOWNLOADS: usize = 8;

/// Marks a `node_modules` tree in the cache as completely installed.
const COMPLETE_MARKER: &str = ".eryx-complete";

/// The registry packages come from, and the hosts it may serve them from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    url: String,
    hosts: Vec<String>,
}

impl Registry {
    /// The public npm registry, `https://registry.npmjs.org/`.
    #[must_use]
    pub fn npmjs() -> Self {
        Self {
            url: "https://registry.npmjs.org/".to_string(),
            hosts: vec!["registry.npmjs.org".to_string()],
        }
    }

    /// A registry at `url` (e.g. a private mirror). Metadata and tarballs are
    /// only fetched from its host, unless more are allowed with
    /// [`allow_tarball_host`](Self::allow_tarball_host).
    ///
    /// # Errors
    ///
    /// Fails unless `url` is an `https` URL with a host.
    pub fn new(url: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).map_err(|_| Error::Fetch {
            url: "registry URL".to_string(),
            message: "not a valid URL".to_string(),
        })?;
        let host = parsed.host_str().filter(|_| parsed.scheme() == "https");
        let Some(host) = host else {
            return Err(Error::Fetch {
                url: "registry URL".to_string(),
                message: "a registry URL must be https".to_string(),
            });
        };
        // Credentials would end up in lockfiles and error messages, and
        // there's no registry auth yet; nor does a registry URL take a query
        // or fragment. The URL itself isn't echoed back, as it may hold a token.
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::Fetch {
                url: format!("https://{host}/"),
                message: "a registry URL can't have credentials, a query or a fragment".to_string(),
            });
        }
        let mut url = parsed.to_string();
        if !url.ends_with('/') {
            url.push('/');
        }
        Ok(Self {
            url,
            hosts: vec![host.to_ascii_lowercase()],
        })
    }

    /// Also download tarballs from `host`, for registries that serve them
    /// from a CDN.
    #[must_use]
    pub fn allow_tarball_host(mut self, host: impl Into<String>) -> Self {
        self.hosts.push(host.into().to_ascii_lowercase());
        self
    }

    /// The registry's URL, ending in `/`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The hosts it may be contacted on.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// An [`HttpFetch`] allowed exactly this registry's hosts.
    ///
    /// # Errors
    ///
    /// Fails if the HTTP client can't be built.
    pub fn http_fetch(&self) -> Result<HttpFetch> {
        HttpFetch::new(self.hosts.iter().cloned())
    }

    pub(crate) fn packument_url(&self, name: &str) -> String {
        format!("{}{}", self.url, name.replace('/', "%2f"))
    }

    pub(crate) fn allows(&self, url: &str) -> bool {
        reqwest::Url::parse(url).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str()
                    .is_some_and(|h| self.hosts.iter().any(|a| a.eq_ignore_ascii_case(h)))
        })
    }
}

/// Caps on what an install may fetch and unpack.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Limits {
    /// Packages in the tree.
    pub max_packages: usize,
    /// Bytes of one metadata document.
    pub max_metadata_bytes: u64,
    /// Bytes of one tarball.
    pub max_tarball_bytes: u64,
    /// Bytes unpacked across the whole tree.
    pub max_total_bytes: u64,
    /// What one tarball may unpack to.
    pub extract: ExtractLimits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_packages: 500,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_tarball_bytes: 32 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            extract: ExtractLimits::default(),
        }
    }
}

/// An installed package set.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Installed {
    /// What was installed.
    pub lockfile: Lockfile,
    /// The `node_modules` directory, in the cache. Treat it as read-only:
    /// other installs of the same lockfile share it.
    pub node_modules: PathBuf,
    /// Whether the packages were resolved afresh (no usable lockfile).
    pub resolved: bool,
    /// Things skipped along the way (optional dependencies, conflicting peers).
    pub warnings: Vec<String>,
}

/// Resolves and installs npm packages.
#[derive(Debug, Clone)]
#[must_use]
pub struct Install<F = HttpFetch> {
    specs: Vec<String>,
    cache_dir: PathBuf,
    registry: Registry,
    lockfile: Option<PathBuf>,
    frozen: bool,
    offline: bool,
    min_release_age: Duration,
    metadata_ttl: Duration,
    limits: Limits,
    now: Option<SystemTime>,
    fetch: Option<F>,
}

impl Install<HttpFetch> {
    /// Install `specs` (`zod`, `zod@^3`, `@scope/pkg@next`), caching
    /// downloads and installed trees under `cache_dir`.
    pub fn new(
        specs: impl IntoIterator<Item = impl Into<String>>,
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            specs: specs.into_iter().map(Into::into).collect(),
            cache_dir: cache_dir.into(),
            registry: Registry::npmjs(),
            lockfile: None,
            frozen: false,
            offline: false,
            min_release_age: Duration::from_secs(3 * 86_400),
            metadata_ttl: Duration::from_secs(5 * 60),
            limits: Limits::default(),
            now: None,
            fetch: None,
        }
    }
}

impl<F: Fetch> Install<F> {
    /// Fetch through `fetch` instead of HTTPS (tests, custom transports). It
    /// is still only asked for URLs on the registry's hosts.
    pub fn with_fetch<G: Fetch>(self, fetch: G) -> Install<G> {
        Install {
            specs: self.specs,
            cache_dir: self.cache_dir,
            registry: self.registry,
            lockfile: self.lockfile,
            frozen: self.frozen,
            offline: self.offline,
            min_release_age: self.min_release_age,
            metadata_ttl: self.metadata_ttl,
            limits: self.limits,
            now: self.now,
            fetch: Some(fetch),
        }
    }

    /// Resolve against `registry` instead of the public npm registry.
    pub fn registry(mut self, registry: Registry) -> Self {
        self.registry = registry;
        self
    }

    /// Use `path` as the lockfile: install exactly what it records if it
    /// was resolved from the same specs and registry, otherwise resolve and
    /// write it.
    pub fn lockfile(mut self, path: impl Into<PathBuf>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    /// Fail instead of resolving when the lockfile is missing or out of date
    /// (as `npm ci` does).
    pub fn frozen(mut self, frozen: bool) -> Self {
        self.frozen = frozen;
        self
    }

    /// Use only the lockfile and the cache; never touch the network.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Don't install versions published less than `age` ago (default 3 days).
    /// Zero turns it off.
    pub fn min_release_age(mut self, age: Duration) -> Self {
        self.min_release_age = age;
        self
    }

    /// How long cached registry metadata is used before it's fetched again
    /// (default 5 minutes). Offline, it's used however old.
    pub fn metadata_ttl(mut self, ttl: Duration) -> Self {
        self.metadata_ttl = ttl;
        self
    }

    /// Replace the default [`Limits`].
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Resolve as of `now` instead of the current time (for the minimum
    /// release age), e.g. to reproduce an earlier resolution.
    pub fn at_time(mut self, now: SystemTime) -> Self {
        self.now = Some(now);
        self
    }

    /// Resolve (unless the lockfile can be used) and install.
    ///
    /// # Errors
    ///
    /// See [`Error`]: a spec that can't be resolved, a refused or failed
    /// download, an integrity mismatch, an unsafe archive, a limit, or
    /// offline mode needing the network.
    pub async fn run(self) -> Result<Installed>
    where
        F: Clone,
    {
        let fetch = match &self.fetch {
            Some(fetch) => FetchRef::Custom(fetch.clone()),
            None if self.offline => FetchRef::None,
            None => FetchRef::Http(self.registry.http_fetch()?),
        };
        let specs = self
            .specs
            .iter()
            .map(|s| Spec::parse(s))
            .collect::<Result<Vec<_>>>()?;
        let requested = resolve::requested(&specs);

        let existing = match &self.lockfile {
            Some(path) if path.exists() => Some(Lockfile::read(path)?),
            _ => None,
        };
        let usable = existing
            .filter(|lock| lock.requested == requested && lock.registry == self.registry.url());
        let (lockfile, resolved, warnings) = match usable {
            Some(lock) => (lock, false, Vec::new()),
            None if self.frozen => {
                return Err(Error::Lockfile(
                    "frozen, but the lockfile is missing or was resolved from other packages"
                        .to_string(),
                ));
            }
            None => {
                let packuments = packument::Packuments {
                    fetch: &fetch,
                    registry: &self.registry,
                    cache_dir: self.cache_dir.join("npm").join("metadata"),
                    ttl: self.metadata_ttl,
                    offline: self.offline,
                    max_bytes: self.limits.max_metadata_bytes,
                };
                let resolver = resolve::Resolver {
                    packuments: &packuments,
                    min_release_age: self.min_release_age,
                    now: self.now.unwrap_or_else(SystemTime::now),
                    max_packages: self.limits.max_packages,
                };
                let resolved = resolver.resolve(&specs).await?;
                // Before writing it: a lockfile that fails validation would
                // make every later run fail to read it.
                resolved.lockfile.validate()?;
                if let Some(path) = &self.lockfile {
                    resolved.lockfile.write(path)?;
                }
                (resolved.lockfile, true, resolved.warnings)
            }
        };
        if lockfile.packages.len() > self.limits.max_packages {
            return Err(Error::Limit(format!(
                "more than {} packages to install",
                self.limits.max_packages
            )));
        }

        let node_modules = self.install_tree(&fetch, &lockfile).await?;
        Ok(Installed {
            lockfile,
            node_modules,
            resolved,
            warnings,
        })
    }

    /// Unpack the lockfile's packages into a tree in the cache named for its
    /// content, unless one is already complete there.
    async fn install_tree(&self, fetch: &FetchRef<F>, lockfile: &Lockfile) -> Result<PathBuf> {
        lockfile.validate()?;
        let trees = self.cache_dir.join("npm").join("trees");
        let tree = trees.join(lockfile.content_hash());
        let node_modules = tree.join("node_modules");
        if tree.join(COMPLETE_MARKER).exists() {
            return Ok(node_modules);
        }

        std::fs::create_dir_all(&trees)
            .map_err(Error::io(format!("creating {}", trees.display())))?;
        let staging = tempfile::tempdir_in(&trees).map_err(Error::io(format!(
            "creating a directory in {}",
            trees.display()
        )))?;

        // Downloads run concurrently but are unpacked one at a time in
        // lockfile (path) order as they arrive, so the tree is the same
        // whatever order they finish in, and at most CONCURRENT_DOWNLOADS
        // tarballs are held in memory.
        let blobs = Blobs::new(self.cache_dir.join("blobs"));
        let mut tarballs = futures::stream::iter(&lockfile.packages)
            .map(|(path, package)| {
                let blobs = &blobs;
                async move { (path, self.tarball(fetch, blobs, package).await) }
            })
            .buffered(CONCURRENT_DOWNLOADS);
        let mut total = 0u64;
        while let Some((path, tarball)) = tarballs.next().await {
            let tarball = tarball?;
            let dest = staging.path().join(path);
            std::fs::create_dir_all(&dest)
                .map_err(Error::io(format!("creating {}", dest.display())))?;
            total += unpack_tgz_stripped(path, &tarball, &dest, self.limits.extract)?;
            if total > self.limits.max_total_bytes {
                return Err(Error::Limit(format!(
                    "the packages unpack to more than {} bytes",
                    self.limits.max_total_bytes
                )));
            }
        }
        std::fs::create_dir_all(staging.path().join("node_modules"))
            .map_err(Error::io("creating node_modules"))?;
        std::fs::write(staging.path().join(COMPLETE_MARKER), b"")
            .map_err(Error::io("marking the install complete"))?;
        let staging = staging.keep();
        if let Err(e) = std::fs::rename(&staging, &tree) {
            // Another install of the same lockfile got there first.
            let _ = std::fs::remove_dir_all(&staging);
            if !tree.join(COMPLETE_MARKER).exists() {
                return Err(Error::io(format!("installing into {}", tree.display()))(e));
            }
        }
        Ok(node_modules)
    }

    async fn tarball(
        &self,
        fetch: &FetchRef<F>,
        blobs: &Blobs,
        package: &LockedPackage,
    ) -> Result<Vec<u8>> {
        let integrity = Integrity::parse(&package.integrity)?;
        if let Some(bytes) = blobs.get(&integrity)? {
            return Ok(bytes);
        }
        let what = format!("{}@{}", package.name, package.version);
        if self.offline {
            return Err(Error::Offline(format!("the tarball for {what}")));
        }
        if !self.registry.allows(&package.resolved) {
            return Err(Error::Fetch {
                url: package.resolved.clone(),
                message: "the tarball isn't on an allowed registry host".to_string(),
            });
        }
        let bytes = fetch
            .get(
                &package.resolved,
                "application/octet-stream",
                self.limits.max_tarball_bytes,
            )
            .await?;
        blobs.put(&what, &integrity, &bytes)?;
        Ok(bytes)
    }
}

/// The fetcher an install uses: its own, HTTPS, or none (offline).
enum FetchRef<F> {
    Custom(F),
    Http(HttpFetch),
    None,
}

impl<F: Fetch> Fetch for FetchRef<F> {
    async fn get(&self, url: &str, accept: &str, max_bytes: u64) -> Result<Vec<u8>> {
        match self {
            Self::Custom(fetch) => fetch.get(url, accept, max_bytes).await,
            Self::Http(fetch) => fetch.get(url, accept, max_bytes).await,
            Self::None => Err(Error::Offline(url.to_string())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
