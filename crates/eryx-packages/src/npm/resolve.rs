//! Resolving requested packages to a `node_modules` layout.
//!
//! Like npm (v7+): each dependency is placed as close to the root as it can
//! go without conflicting with one already on its resolution path, reusing
//! any version there that satisfies it; only a conflicting version is nested
//! under the package that needs it. Missing peer dependencies are installed
//! next to the package that declares them; optional dependencies that are
//! platform binaries (`os`/`cpu`) are skipped, since a sandbox can't run them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt as _;
use nodejs_semver::{Range, Version};

use super::lockfile::{LOCKFILE_VERSION, LockedPackage, Lockfile};
use super::packument::{Manifest, Packument, Packuments};
use super::spec::Spec;
use crate::fetch::Fetch;
use crate::{Error, Result};

/// Packuments fetched at once.
const CONCURRENT_FETCHES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    Optional,
    Peer,
}

/// A dependency to place, wanted by the package at node `requester`.
struct Want {
    requester: usize,
    name: String,
    range: String,
    kind: Kind,
}

struct Node {
    name: String,
    version: String,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
    manifest: Option<Manifest>,
}

pub(crate) struct Resolver<'a, F> {
    pub(crate) packuments: &'a Packuments<'a, F>,
    pub(crate) min_release_age: Duration,
    pub(crate) now: SystemTime,
    pub(crate) max_packages: usize,
}

pub(crate) struct Resolved {
    pub(crate) lockfile: Lockfile,
    pub(crate) warnings: Vec<String>,
}

impl<F: Fetch> Resolver<'_, F> {
    pub(crate) async fn resolve(&self, specs: &[Spec]) -> Result<Resolved> {
        let mut state = State {
            nodes: vec![Node {
                name: String::new(),
                version: String::new(),
                parent: None,
                children: BTreeMap::new(),
                manifest: None,
            }],
            packuments: HashMap::new(),
            unavailable: HashSet::new(),
            warnings: Vec::new(),
        };
        let mut queue: Vec<Want> = specs
            .iter()
            .map(|spec| Want {
                requester: 0,
                name: spec.name.clone(),
                range: spec.version.clone(),
                kind: Kind::Normal,
            })
            .collect();

        while !queue.is_empty() {
            self.fetch_missing(&mut state, &queue).await?;
            let mut next = Vec::new();
            for want in queue {
                self.place(&mut state, want, &mut next)?;
            }
            queue = next;
        }

        let lockfile = state.lockfile(self.packuments.registry.url(), specs)?;
        Ok(Resolved {
            lockfile,
            warnings: state.warnings,
        })
    }

    /// Fetch, concurrently, the packuments `wave` needs that aren't in yet.
    async fn fetch_missing(&self, state: &mut State, wave: &[Want]) -> Result<()> {
        let mut names: Vec<(&str, Kind)> = Vec::new();
        for want in wave {
            if !state.packuments.contains_key(&want.name)
                && !state.unavailable.contains(&want.name)
                && !names.iter().any(|(n, _)| *n == want.name)
            {
                names.push((&want.name, want.kind));
            }
        }
        let fetched: Vec<_> = futures::stream::iter(names)
            .map(|(name, kind)| async move { (name, kind, self.packuments.get(name).await) })
            .buffer_unordered(CONCURRENT_FETCHES)
            .collect()
            .await;
        for (name, kind, result) in fetched {
            match result {
                Ok(packument) => {
                    state
                        .packuments
                        .insert(name.to_string(), Arc::new(packument));
                }
                Err(e) if kind == Kind::Optional => {
                    state
                        .warnings
                        .push(format!("skipped optional dependency {name}: {e}"));
                    state.unavailable.insert(name.to_string());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn place(&self, state: &mut State, want: Want, next: &mut Vec<Want>) -> Result<()> {
        let spec = format!("{}@{}", want.name, want.range);
        let Some(packument) = state.packuments.get(&want.name).cloned() else {
            return Ok(()); // An optional dependency that couldn't be fetched.
        };

        // Walk from the requester up to the root: reuse a satisfying version
        // on the way, otherwise take the highest free level before the first
        // conflicting one.
        let mut free = None;
        let mut level = Some(want.requester);
        while let Some(at) = level {
            if let Some(&existing) = state.nodes[at].children.get(&want.name) {
                if satisfies(&packument, &want.range, &state.nodes[existing].version) {
                    return Ok(());
                }
                break;
            }
            free = Some(at);
            level = state.nodes[at].parent;
        }
        let Some(level) = free else {
            let message = "conflicts with another version already installed for the same package";
            if want.kind == Kind::Normal {
                return Err(Error::Resolve {
                    spec,
                    message: message.to_string(),
                });
            }
            state.warnings.push(format!("skipped {spec}: {message}"));
            return Ok(());
        };

        let version = match self.select(&want.name, &want.range, &packument) {
            Ok(version) => version,
            Err(e) if want.kind == Kind::Optional => {
                state
                    .warnings
                    .push(format!("skipped optional dependency {spec}: {e}"));
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let manifest = packument.versions[&version].clone();
        if want.kind == Kind::Optional && (!manifest.os.is_empty() || !manifest.cpu.is_empty()) {
            return Ok(()); // A platform binary.
        }
        if state.nodes.len() > self.max_packages {
            return Err(Error::Limit(format!(
                "more than {} packages to install",
                self.max_packages
            )));
        }

        let id = state.nodes.len();
        let deps = [
            (Kind::Normal, &manifest.dependencies),
            (Kind::Optional, &manifest.optional_dependencies),
        ];
        for (kind, map) in deps {
            for (name, range) in map {
                // An optional dependency also listed as a regular one is optional.
                if kind == Kind::Normal && manifest.optional_dependencies.contains_key(name) {
                    continue;
                }
                next.push(Want {
                    requester: id,
                    name: name.clone(),
                    range: range.clone(),
                    kind,
                });
            }
        }
        for (name, range) in &manifest.peer_dependencies {
            let optional = manifest
                .peer_dependencies_meta
                .get(name)
                .is_some_and(|m| m.optional);
            if !optional {
                // Peers resolve from where the package itself sits.
                next.push(Want {
                    requester: level,
                    name: name.clone(),
                    range: range.clone(),
                    kind: Kind::Peer,
                });
            }
        }
        state.nodes[level].children.insert(want.name.clone(), id);
        state.nodes.push(Node {
            name: want.name,
            version,
            parent: Some(level),
            children: BTreeMap::new(),
            manifest: Some(manifest),
        });
        Ok(())
    }

    /// The version of `name` to install for `range`: the `latest` tag if it
    /// satisfies, otherwise the highest satisfying one, among versions
    /// published at least `min_release_age` ago.
    fn select(&self, name: &str, range: &str, packument: &Packument) -> Result<String> {
        let spec = format!("{name}@{range}");
        let error = |message: String| Error::Resolve {
            spec: spec.clone(),
            message,
        };
        let cutoff = self
            .now
            .checked_sub(self.min_release_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let old_enough = |version: &str| {
            self.min_release_age.is_zero()
                || packument.published(version).is_some_and(|t| t <= cutoff)
        };
        let age = || {
            let days = self.min_release_age.as_secs() / 86_400;
            format!(
                "it was published less than {days} day(s) ago (the minimum release age), or \
                 the registry doesn't say when"
            )
        };

        let Ok(parsed) = Range::parse(range) else {
            // Not a range: a dist-tag.
            let version = packument
                .dist_tags
                .get(range)
                .ok_or_else(|| error(format!("no version or dist-tag {range:?}")))?;
            if !packument.versions.contains_key(version) {
                return Err(error(format!(
                    "dist-tag {range} points at a missing version"
                )));
            }
            if old_enough(version) {
                return Ok(version.clone());
            }
            // `latest` (what a bare name asks for) falls back to the newest
            // release before it that's old enough; other tags name one version.
            let newest_eligible = (range == "latest")
                .then(|| Version::parse(version).ok())
                .flatten()
                .and_then(|latest| {
                    packument
                        .versions
                        .keys()
                        .filter_map(|v| Version::parse(v).ok())
                        .filter(|v| !v.is_prerelease() && *v <= latest)
                        .filter(|v| old_enough(&v.to_string()))
                        .max()
                });
            return newest_eligible
                .map(|v| v.to_string())
                .filter(|v| packument.versions.contains_key(v))
                .ok_or_else(|| error(format!("{range} is {version}, but {}", age())));
        };

        let matching: Vec<Version> = packument
            .versions
            .keys()
            .filter_map(|v| Version::parse(v).ok())
            .filter(|v| parsed.satisfies(v))
            .collect();
        if matching.is_empty() {
            return Err(error("no version satisfies it".to_string()));
        }
        if let Some(latest) = packument.dist_tags.get("latest")
            && let Ok(v) = Version::parse(latest)
            && parsed.satisfies(&v)
            && old_enough(latest)
            && packument.versions.contains_key(latest)
        {
            return Ok(latest.clone());
        }
        matching
            .iter()
            .filter(|v| old_enough(&v.to_string()))
            .max()
            .map(ToString::to_string)
            .filter(|v| packument.versions.contains_key(v))
            .ok_or_else(|| {
                let newest = matching
                    .iter()
                    .max()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                error(format!("the newest match is {newest}, but {}", age()))
            })
    }
}

/// Whether the installed `version` satisfies `range` (or is what the
/// dist-tag `range` points at).
fn satisfies(packument: &Packument, range: &str, version: &str) -> bool {
    let Ok(version) = Version::parse(version) else {
        return false;
    };
    match Range::parse(range) {
        Ok(range) => range.satisfies(&version),
        Err(_) => packument
            .dist_tags
            .get(range)
            .is_some_and(|tagged| Version::parse(tagged).is_ok_and(|t| t == version)),
    }
}

struct State {
    nodes: Vec<Node>,
    packuments: HashMap<String, Arc<Packument>>,
    unavailable: HashSet<String>,
    warnings: Vec<String>,
}

impl State {
    fn path(&self, id: usize) -> String {
        let node = &self.nodes[id];
        match node.parent {
            Some(0) | None => format!("node_modules/{}", node.name),
            Some(parent) => format!("{}/node_modules/{}", self.path(parent), node.name),
        }
    }

    fn lockfile(&self, registry: &str, specs: &[Spec]) -> Result<Lockfile> {
        let mut packages = BTreeMap::new();
        for (id, node) in self.nodes.iter().enumerate().skip(1) {
            let Some(manifest) = &node.manifest else {
                continue;
            };
            let integrity = manifest
                .dist
                .integrity
                .clone()
                .ok_or_else(|| Error::Resolve {
                    spec: format!("{}@{}", node.name, node.version),
                    message: "the registry gives no SHA-512 integrity hash for it".to_string(),
                })?;
            packages.insert(
                self.path(id),
                LockedPackage {
                    name: node.name.clone(),
                    version: node.version.clone(),
                    resolved: manifest.dist.tarball.clone(),
                    integrity,
                    dependencies: manifest.dependencies.clone(),
                    optional_dependencies: manifest.optional_dependencies.clone(),
                    peer_dependencies: manifest.peer_dependencies.clone(),
                },
            );
        }
        Ok(Lockfile {
            lockfile_version: LOCKFILE_VERSION,
            registry: registry.to_string(),
            requested: requested(specs),
            packages,
        })
    }
}

/// The specs as a lockfile records them: sorted `name@version` strings.
pub(crate) fn requested(specs: &[Spec]) -> Vec<String> {
    let mut requested: Vec<String> = specs.iter().map(ToString::to_string).collect();
    requested.sort();
    requested.dedup();
    requested
}
