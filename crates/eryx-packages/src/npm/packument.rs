//! Registry metadata documents ("packuments"), fetched and cached.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Registry;
use crate::fetch::Fetch;
use crate::{Error, Result};

/// The parts of a packument resolution needs.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Packument {
    #[serde(default, rename = "dist-tags")]
    pub(crate) dist_tags: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) versions: BTreeMap<String, Manifest>,
    /// Publish times by version (plus `created` and `modified`).
    #[serde(default)]
    pub(crate) time: BTreeMap<String, String>,
}

/// One version's manifest.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    #[serde(default)]
    pub(crate) dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    pub(crate) optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    pub(crate) peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependenciesMeta")]
    pub(crate) peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    /// Platform restrictions: a package with any is a native binary.
    #[serde(default)]
    pub(crate) os: Vec<String>,
    #[serde(default)]
    pub(crate) cpu: Vec<String>,
    pub(crate) dist: Dist,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PeerMeta {
    #[serde(default)]
    pub(crate) optional: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Dist {
    pub(crate) tarball: String,
    #[serde(default)]
    pub(crate) integrity: Option<String>,
}

impl Packument {
    /// When `version` was published, if the registry says.
    pub(crate) fn published(&self, version: &str) -> Option<SystemTime> {
        parse_rfc3339(self.time.get(version)?)
    }
}

/// A cached packument: when it was fetched, and the raw document.
#[derive(Serialize, Deserialize)]
struct CachedDocument {
    fetched_at: u64,
    body: String,
}

/// Fetches packuments, keeping a copy of each in the cache for `ttl`.
pub(crate) struct Packuments<'a, F> {
    pub(crate) fetch: &'a F,
    pub(crate) registry: &'a Registry,
    pub(crate) cache_dir: PathBuf,
    pub(crate) ttl: Duration,
    pub(crate) offline: bool,
    pub(crate) max_bytes: u64,
}

impl<F: Fetch> Packuments<'_, F> {
    pub(crate) async fn get(&self, name: &str) -> Result<Packument> {
        let path = self.cache_path(name);
        let cached = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedDocument>(&bytes).ok());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let body = match cached {
            Some(doc)
                if self.offline || now.saturating_sub(doc.fetched_at) < self.ttl.as_secs() =>
            {
                doc.body
            }
            _ if self.offline => return Err(Error::Offline(format!("metadata for {name}"))),
            _ => {
                let url = self.registry.packument_url(name);
                // The full document: the abbreviated one has no publish times.
                let bytes = self
                    .fetch
                    .get(&url, "application/json", self.max_bytes)
                    .await?;
                let body = String::from_utf8(bytes).map_err(|_| Error::Fetch {
                    url: url.clone(),
                    message: "metadata isn't UTF-8".to_string(),
                })?;
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let doc = CachedDocument {
                    fetched_at: now,
                    body,
                };
                // Best effort: a cache that can't be written just isn't used.
                if let Ok(json) = serde_json::to_vec(&doc) {
                    let _ = write_atomic(&path, &json);
                }
                doc.body
            }
        };
        serde_json::from_str(&body).map_err(|e| Error::Fetch {
            url: self.registry.packument_url(name),
            message: format!("bad metadata: {e}"),
        })
    }

    fn cache_path(&self, name: &str) -> PathBuf {
        let registry = hex::encode(&Sha256::digest(self.registry.url().as_bytes())[..8]);
        self.cache_dir
            .join(registry)
            .join(format!("{}.json", name.replace('/', "%2f")))
    }
}

pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(tmp.path(), bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Parse an RFC 3339 UTC timestamp as npm writes them
/// (`2024-01-02T03:04:05.678Z`). Offsets other than `Z` aren't accepted.
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time = time.splitn(3, ':').map(str::parse::<u64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis: u64 = fraction
        .get(..fraction.len().min(3))
        .filter(|f| !f.is_empty())
        .map_or(Some(0), |f| format!("{f:0<3}").parse().ok())?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = u64::try_from(era * 146_097 + doe - 719_468).ok()?;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::from_millis(secs * 1_000 + millis))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_npm_timestamps() {
        let at =
            |s: &str| parse_rfc3339(s).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_millis());
        assert_eq!(at("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(at("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(at("2024-02-29T12:34:56.789Z"), Some(1_709_210_096_789));
        assert_eq!(at("2024-02-29T12:34:56.7Z"), Some(1_709_210_096_700));
        for bad in [
            "",
            "2024-02-29",
            "2024-13-01T00:00:00Z",
            "2024-01-01T00:00:00+01:00",
        ] {
            assert_eq!(at(bad), None, "{bad}");
        }
    }
}
