//! Resolution and install against an in-memory registry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::*;
use crate::extract::tests::tgz;

const REGISTRY: &str = "https://registry.test/";
const DAY: u64 = 86_400;

/// "Now" for these tests: 2024-01-31.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_706_659_200)
}

fn days_ago(days: u64) -> String {
    let secs = now().duration_since(UNIX_EPOCH).unwrap().as_secs() - days * DAY;
    // Civil date from days (inverse of `parse_rfc3339`), good enough for tests.
    let days = (secs / DAY) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z")
}

/// A fake registry: packuments and tarballs by URL, and a log of requests.
#[derive(Clone, Default)]
struct Fake {
    docs: Arc<Mutex<HashMap<String, Value>>>,
    tarballs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// Publish `name@version`, `age` days ago, with `manifest` fields
    /// (`dependencies` and so on) merged in.
    fn publish(&self, name: &str, version: &str, age: u64, manifest: Value) -> &Self {
        let tarball = tgz(&[
            (
                "package/package.json",
                format!(r#"{{"name":"{name}","version":"{version}"}}"#).as_bytes(),
            ),
            (
                "package/index.js",
                format!("module.exports = '{name}@{version}';").as_bytes(),
            ),
        ]);
        let url = format!(
            "{REGISTRY}{name}/-/{}-{version}.tgz",
            name.rsplit('/').next().unwrap()
        );
        let integrity = crate::integrity::Integrity::sha512_of(&tarball).to_string();
        self.tarballs.lock().unwrap().insert(url.clone(), tarball);

        let mut entry = json!({ "name": name, "version": version,
            "dist": { "tarball": url, "integrity": integrity } });
        if let Value::Object(fields) = manifest {
            entry.as_object_mut().unwrap().extend(fields);
        }
        let mut docs = self.docs.lock().unwrap();
        let doc = docs.entry(name.to_string()).or_insert_with(
            || json!({ "name": name, "dist-tags": {}, "versions": {}, "time": {} }),
        );
        doc["versions"][version] = entry;
        doc["time"][version] = json!(days_ago(age));
        if !version.contains('-') {
            doc["dist-tags"]["latest"] = json!(version);
        }
        self
    }

    fn tag(&self, name: &str, tag: &str, version: &str) -> &Self {
        self.docs.lock().unwrap().get_mut(name).unwrap()["dist-tags"][tag] = json!(version);
        self
    }

    fn requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

impl crate::fetch::Fetch for Fake {
    async fn get(&self, url: &str, _accept: &str, max_bytes: u64) -> Result<Vec<u8>> {
        self.requests.lock().unwrap().push(url.to_string());
        let not_found = || Error::Fetch {
            url: url.to_string(),
            message: "HTTP 404".to_string(),
        };
        let bytes = if let Some(name) = url.strip_prefix(REGISTRY).filter(|n| !n.contains("/-/")) {
            let name = name.replace("%2f", "/");
            serde_json::to_vec(self.docs.lock().unwrap().get(&name).ok_or_else(not_found)?).unwrap()
        } else {
            self.tarballs
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(not_found)?
        };
        if bytes.len() as u64 > max_bytes {
            return Err(Error::Limit(url.to_string()));
        }
        Ok(bytes)
    }
}

fn install(fake: &Fake, cache: &std::path::Path, specs: &[&str]) -> Install<Fake> {
    Install::new(specs.iter().copied(), cache)
        .registry(Registry::new(REGISTRY).unwrap())
        .with_fetch(fake.clone())
        .at_time(now())
}

fn versions(installed: &Installed) -> Vec<(String, String)> {
    installed
        .lockfile
        .packages
        .iter()
        .map(|(path, p)| (path.clone(), p.version.clone()))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
        .collect()
}

#[tokio::test]
async fn hoists_shared_dependencies_and_nests_conflicts() {
    let fake = Fake::default();
    fake.publish("c", "1.5.0", 30, json!({}))
        .publish("c", "2.1.0", 30, json!({}))
        .publish("a", "1.0.0", 30, json!({ "dependencies": { "c": "^1.2" } }))
        .publish(
            "b",
            "1.0.0",
            30,
            json!({ "dependencies": { "c": "^2", "a": "*" } }),
        );
    let cache = tempfile::tempdir().unwrap();

    let installed = install(&fake, cache.path(), &["a", "b@^1"])
        .run()
        .await
        .unwrap();

    assert!(installed.resolved);
    assert_eq!(
        versions(&installed),
        pairs(&[
            ("node_modules/a", "1.0.0"),
            ("node_modules/b", "1.0.0"),
            ("node_modules/b/node_modules/c", "2.1.0"),
            ("node_modules/c", "1.5.0"),
        ])
    );
    let nested = installed.node_modules.join("b/node_modules/c/index.js");
    assert_eq!(
        std::fs::read_to_string(nested).unwrap(),
        "module.exports = 'c@2.1.0';"
    );
    assert!(installed.node_modules.join("a/package.json").exists());
}

#[tokio::test]
async fn picks_latest_or_the_highest_match_skipping_prereleases() {
    let fake = Fake::default();
    fake.publish("x", "1.0.0", 30, json!({}))
        .publish("x", "1.4.0", 30, json!({}))
        .publish("x", "2.0.0", 30, json!({}))
        .publish("x", "3.0.0-beta.1", 30, json!({}))
        .tag("x", "latest", "1.4.0")
        .tag("x", "next", "3.0.0-beta.1");
    let cache = tempfile::tempdir().unwrap();
    let version = |installed: Installed| {
        installed.lockfile.packages["node_modules/x"]
            .version
            .clone()
    };

    assert_eq!(
        version(install(&fake, cache.path(), &["x"]).run().await.unwrap()),
        "1.4.0"
    );
    assert_eq!(
        version(install(&fake, cache.path(), &["x@*"]).run().await.unwrap()),
        "1.4.0"
    );
    assert_eq!(
        version(
            install(&fake, cache.path(), &["x@>=2"])
                .run()
                .await
                .unwrap()
        ),
        "2.0.0"
    );
    assert_eq!(
        version(
            install(&fake, cache.path(), &["x@next"])
                .run()
                .await
                .unwrap()
        ),
        "3.0.0-beta.1"
    );
    let err = install(&fake, cache.path(), &["x@^4"])
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Resolve { .. }), "{err}");
    let err = install(&fake, cache.path(), &["x@nope"])
        .run()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("dist-tag"), "{err}");
}

#[tokio::test]
async fn minimum_release_age() {
    let fake = Fake::default();
    fake.publish("x", "1.0.0", 30, json!({}))
        .publish("x", "1.1.0", 1, json!({}))
        .publish("y", "1.0.0", 1, json!({}));
    let cache = tempfile::tempdir().unwrap();

    // The default of 3 days skips the release from yesterday...
    let installed = install(&fake, cache.path(), &["x"]).run().await.unwrap();
    assert_eq!(
        installed.lockfile.packages["node_modules/x"].version,
        "1.0.0"
    );
    let installed = install(&fake, cache.path(), &["x@^1"]).run().await.unwrap();
    assert_eq!(
        installed.lockfile.packages["node_modules/x"].version,
        "1.0.0"
    );
    // ...and refuses when nothing else matches...
    let err = install(&fake, cache.path(), &["y"])
        .run()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("minimum release age"), "{err}");
    let err = install(&fake, cache.path(), &["x@1.1.0"])
        .run()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("newest match is 1.1.0"), "{err}");
    // ...unless it's turned down.
    let installed = install(&fake, cache.path(), &["x@^1"])
        .min_release_age(Duration::ZERO)
        .run()
        .await
        .unwrap();
    assert_eq!(
        installed.lockfile.packages["node_modules/x"].version,
        "1.1.0"
    );
}

#[tokio::test]
async fn lockfile_reuse_offline_and_frozen() {
    let fake = Fake::default();
    fake.publish("a", "1.0.0", 30, json!({ "dependencies": { "b": "^1" } }))
        .publish("b", "1.0.0", 30, json!({}));
    let cache = tempfile::tempdir().unwrap();
    let lock = cache.path().join("eryx-npm.lock");

    let first = install(&fake, cache.path(), &["a"])
        .lockfile(&lock)
        .run()
        .await
        .unwrap();
    assert!(first.resolved && lock.exists());
    assert_eq!(fake.requests().len(), 4, "two packuments and two tarballs");

    // A newer b doesn't change a locked install, which fetches nothing.
    fake.publish("b", "1.9.0", 30, json!({}));
    let again = install(&fake, cache.path(), &["a"])
        .lockfile(&lock)
        .run()
        .await
        .unwrap();
    assert!(!again.resolved);
    assert_eq!(again.lockfile, first.lockfile);
    assert_eq!(again.node_modules, first.node_modules);
    assert_eq!(fake.requests(), Vec::<String>::new());

    // Offline from a fresh cache directory fails; from this one it works.
    let offline = install(&fake, cache.path(), &["a"])
        .lockfile(&lock)
        .offline(true);
    offline.run().await.unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let err = install(&fake, elsewhere.path(), &["a"])
        .lockfile(&lock)
        .offline(true)
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Offline(_)), "{err}");

    // Asking for other packages re-resolves, unless frozen.
    let err = install(&fake, cache.path(), &["a", "b"])
        .lockfile(&lock)
        .frozen(true)
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Lockfile(_)), "{err}");
    let more = install(&fake, cache.path(), &["a", "b"])
        .lockfile(&lock)
        .run()
        .await
        .unwrap();
    assert!(more.resolved);
    assert_eq!(
        Lockfile::read(&lock).unwrap().requested,
        ["a@latest", "b@latest"]
    );
}

#[tokio::test]
async fn refuses_tampered_tarballs_foreign_hosts_and_missing_integrity() {
    let cache = tempfile::tempdir().unwrap();

    let fake = Fake::default();
    fake.publish("a", "1.0.0", 30, json!({}));
    let url = format!("{REGISTRY}a/-/a-1.0.0.tgz");
    fake.tarballs
        .lock()
        .unwrap()
        .insert(url, tgz(&[("package/index.js", b"evil")]));
    let err = install(&fake, cache.path(), &["a"])
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Integrity { .. }), "{err}");

    let fake = Fake::default();
    fake.publish("b", "1.0.0", 30, json!({}));
    fake.docs.lock().unwrap().get_mut("b").unwrap()["versions"]["1.0.0"]["dist"]["tarball"] =
        json!("https://evil.test/b.tgz");
    let err = install(&fake, cache.path(), &["b"])
        .run()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("allowed registry host"), "{err}");
    assert!(fake.requests().iter().all(|r| !r.contains("evil")));

    let fake = Fake::default();
    fake.publish("c", "1.0.0", 30, json!({}));
    fake.docs.lock().unwrap().get_mut("c").unwrap()["versions"]["1.0.0"]["dist"]
        .as_object_mut()
        .unwrap()
        .remove("integrity");
    let err = install(&fake, cache.path(), &["c"])
        .run()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("integrity"), "{err}");
}

#[tokio::test]
async fn optional_platform_binaries_skipped_and_peers_installed() {
    let fake = Fake::default();
    fake.publish(
        "native-linux",
        "1.0.0",
        30,
        json!({ "os": ["linux"], "cpu": ["x64"] }),
    )
    .publish("helper", "1.0.0", 30, json!({}))
    .publish("react", "18.2.0", 30, json!({}))
    .publish(
        "lib",
        "1.0.0",
        30,
        json!({
            "optionalDependencies": { "native-linux": "1", "helper": "1", "missing": "1" },
            "peerDependencies": { "react": ">=17", "optional-peer": "1" },
            "peerDependenciesMeta": { "optional-peer": { "optional": true } },
        }),
    );
    let cache = tempfile::tempdir().unwrap();

    let installed = install(&fake, cache.path(), &["lib"]).run().await.unwrap();

    assert_eq!(
        versions(&installed),
        pairs(&[
            ("node_modules/helper", "1.0.0"),
            ("node_modules/lib", "1.0.0"),
            ("node_modules/react", "18.2.0"),
        ])
    );
    assert!(
        installed.warnings.iter().any(|w| w.contains("missing")),
        "{:?}",
        installed.warnings
    );
}

#[tokio::test]
async fn limits() {
    let fake = Fake::default();
    fake.publish(
        "a",
        "1.0.0",
        30,
        json!({ "dependencies": { "b": "1", "c": "1" } }),
    )
    .publish("b", "1.0.0", 30, json!({}))
    .publish("c", "1.0.0", 30, json!({}));
    let cache = tempfile::tempdir().unwrap();
    let limits = Limits {
        max_packages: 2,
        ..Limits::default()
    };
    let err = install(&fake, cache.path(), &["a"])
        .limits(limits)
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Limit(_)), "{err}");

    let limits = Limits {
        max_tarball_bytes: 10,
        ..Limits::default()
    };
    let err = install(&fake, cache.path(), &["b"])
        .limits(limits)
        .run()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Limit(_)), "{err}");
}

#[test]
fn registry_urls() {
    assert!(Registry::new("http://registry.test/").is_err());
    assert!(Registry::new("not a url").is_err());
    let registry = Registry::new("https://Mirror.test/npm")
        .unwrap()
        .allow_tarball_host("cdn.test");
    assert_eq!(registry.url(), "https://mirror.test/npm/");
    assert_eq!(
        registry.packument_url("@s/p"),
        "https://mirror.test/npm/@s%2fp"
    );
    assert!(registry.allows("https://cdn.test/p.tgz"));
    assert!(!registry.allows("http://mirror.test/p.tgz"));
    assert!(!registry.allows("https://other.test/p.tgz"));
}
