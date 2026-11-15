//! `eryx-npm.lock`: exactly what was installed, so it can be again.
//!
//! The `packages` map has the shape of npm's `package-lock.json`
//! (`lockfileVersion` 3): keys are install paths (`node_modules/a`,
//! `node_modules/a/node_modules/b`), entries carry `version`, `resolved` and
//! `integrity`, and declared ranges under `dependencies`,
//! `optionalDependencies` and `peerDependencies`. Converting a
//! `package-lock.json` is then mostly copying its `packages` map.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// The lockfile format version this crate writes and reads.
pub const LOCKFILE_VERSION: u32 = 1;

/// A resolved package set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// [`LOCKFILE_VERSION`].
    #[serde(rename = "lockfileVersion")]
    pub lockfile_version: u32,
    /// The registry it was resolved against.
    pub registry: String,
    /// The specs it was resolved from, sorted, e.g. `["date-fns@latest", "zod@^3"]`.
    pub requested: Vec<String>,
    /// Every installed package by install path.
    pub packages: BTreeMap<String, LockedPackage>,
}

/// One installed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    /// The package's name (the last `node_modules/` segment of its path).
    pub name: String,
    /// The exact version.
    pub version: String,
    /// The tarball URL.
    pub resolved: String,
    /// The tarball's SRI hash.
    pub integrity: String,
    /// Declared dependency ranges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    /// Declared optional dependency ranges.
    #[serde(
        default,
        rename = "optionalDependencies",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub optional_dependencies: BTreeMap<String, String>,
    /// Declared peer dependency ranges.
    #[serde(
        default,
        rename = "peerDependencies",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub peer_dependencies: BTreeMap<String, String>,
}

impl Lockfile {
    /// Read a lockfile.
    ///
    /// # Errors
    ///
    /// Fails if it can't be read or parsed, or is a version this crate
    /// doesn't know.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(Error::io(format!("reading {}", path.display())))?;
        let lock: Self = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Lockfile(format!("{}: {e}", path.display())))?;
        if lock.lockfile_version != LOCKFILE_VERSION {
            return Err(Error::Lockfile(format!(
                "{} is lockfile version {}, this eryx reads {LOCKFILE_VERSION}",
                path.display(),
                lock.lockfile_version
            )));
        }
        lock.validate()?;
        Ok(lock)
    }

    /// Write it, pretty-printed with a trailing newline.
    ///
    /// # Errors
    ///
    /// Fails if it can't be written.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Lockfile(format!("serializing: {e}")))?;
        json.push(b'\n');
        std::fs::write(path, json).map_err(Error::io(format!("writing {}", path.display())))
    }

    /// A short hash of what's installed, naming its `node_modules` tree in
    /// the cache.
    #[must_use]
    pub fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, package) in &self.packages {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(package.integrity.as_bytes());
            hasher.update([0]);
        }
        hex::encode(&hasher.finalize()[..16])
    }

    /// Check install paths are well-formed (`node_modules/<name>` segments
    /// only) and match the names they claim, so a hand-edited lockfile can't
    /// install outside its tree.
    pub(crate) fn validate(&self) -> Result<()> {
        for (path, package) in &self.packages {
            let bad = |why: &str| Error::Lockfile(format!("bad package path {path:?}: {why}"));
            let mut rest = path.as_str();
            let mut last = None;
            while !rest.is_empty() {
                rest = rest
                    .strip_prefix("node_modules/")
                    .ok_or_else(|| bad("expected node_modules/"))?;
                let len = if rest.starts_with('@') {
                    let scope = rest.find('/').ok_or_else(|| bad("incomplete scope"))?;
                    scope
                        + 1
                        + rest[scope + 1..]
                            .find('/')
                            .unwrap_or(rest.len() - scope - 1)
                } else {
                    rest.find('/').unwrap_or(rest.len())
                };
                let name = &rest[..len];
                super::spec::check_name(name).map_err(|e| bad(&e))?;
                last = Some(name);
                rest = rest[len..].strip_prefix('/').unwrap_or(&rest[len..]);
            }
            if last != Some(package.name.as_str()) {
                return Err(bad("doesn't end in the package's name"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn package(name: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            resolved: format!("https://r.test/{name}.tgz"),
            integrity: "sha512-x".to_string(),
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
        }
    }

    fn lock(paths: &[(&str, &str)]) -> Lockfile {
        Lockfile {
            lockfile_version: LOCKFILE_VERSION,
            registry: "https://r.test/".to_string(),
            requested: vec![],
            packages: paths
                .iter()
                .map(|(p, n)| ((*p).to_string(), package(n)))
                .collect(),
        }
    }

    #[test]
    fn validates_install_paths() {
        lock(&[
            ("node_modules/a", "a"),
            ("node_modules/a/node_modules/@s/b", "@s/b"),
            ("node_modules/@s/b/node_modules/c", "c"),
        ])
        .validate()
        .unwrap();
        for (path, name) in [
            ("a", "a"),
            ("node_modules/../x", ".."),
            ("node_modules/a/../../x", "x"),
            ("node_modules/a/b", "b"),
            ("node_modules/@s", "@s"),
            ("node_modules/a", "b"),
            ("node_modules/A", "A"),
        ] {
            assert!(lock(&[(path, name)]).validate().is_err(), "{path}");
        }
    }

    #[test]
    fn round_trips_and_hashes_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eryx-npm.lock");
        let original = lock(&[("node_modules/a", "a")]);
        original.write(&path).unwrap();
        assert_eq!(Lockfile::read(&path).unwrap(), original);

        let mut other = original.clone();
        other.packages.get_mut("node_modules/a").unwrap().integrity = "sha512-y".to_string();
        assert_ne!(original.content_hash(), other.content_hash());
    }
}
