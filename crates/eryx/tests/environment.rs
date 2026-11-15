//! Building environments (`eryx::environment`), offline.
//!
//! The packages are the bundler's fixtures (`crates/eryx-bundler/tests/fixture`),
//! packed as tarballs into a cache and pinned by a lockfile, so the build
//! never touches the network: it installs from the cache and bundles with the
//! embedded bundler.
//!
//! ```sh
//! mise run precompile-bundler
//! cargo nextest run -p eryx --features packages --test environment
//! ```
#![cfg(feature = "packages")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eryx::Language;
use eryx::environment::{
    BuildOptions, Environment, EnvironmentDefinition, EnvironmentError, build,
};
use eryx_packages::blobs::Blobs;
use eryx_packages::integrity::Integrity;
use eryx_packages::npm::{LOCKFILE_VERSION, LockedPackage, Lockfile};

const PACKAGES: [&str; 5] = [
    "@scope/sub",
    "cjs-pkg",
    "esm-pkg",
    "events",
    "uses-builtins",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../eryx-bundler/tests/fixture/node_modules")
        .join(name)
}

/// The fixture package `name` as an npm tarball (everything under `package/`).
fn tarball(name: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all("package", fixture(name)).unwrap();
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &tar).unwrap();
    gz.finish().unwrap()
}

/// A cache holding the fixtures' tarballs, and a lockfile pinning them.
fn prepare(dir: &Path) -> (PathBuf, PathBuf) {
    let cache = dir.join("cache");
    let blobs = Blobs::new(cache.join("blobs"));
    let mut packages = BTreeMap::new();
    for name in PACKAGES {
        let bytes = tarball(name);
        let integrity = Integrity::sha512_of(&bytes);
        blobs.put(name, &integrity, &bytes).unwrap();
        packages.insert(
            format!("node_modules/{name}"),
            LockedPackage {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                resolved: format!("https://registry.npmjs.org/{name}/-/x-1.0.0.tgz"),
                integrity: integrity.to_string(),
                dependencies: BTreeMap::new(),
                optional_dependencies: BTreeMap::new(),
                peer_dependencies: BTreeMap::new(),
            },
        );
    }
    let lockfile = Lockfile {
        lockfile_version: LOCKFILE_VERSION,
        registry: "https://registry.npmjs.org/".to_string(),
        requested: vec![
            "@scope/sub@1".to_string(),
            "esm-pkg@1".to_string(),
            "uses-builtins@1".to_string(),
        ],
        packages,
    };
    let path = dir.join("fixtures.lock");
    lockfile.write(&path).unwrap();
    (cache, path)
}

fn definition() -> EnvironmentDefinition {
    EnvironmentDefinition::from_toml(
        r#"
        name = "fixtures"
        language = "javascript"

        [npm]
        packages = ["esm-pkg@1", "uses-builtins@1", "@scope/sub@1"]
        entries = ["esm-pkg/extra"]
        "#,
    )
    .unwrap()
}

fn options(dir: &Path, cache: &Path, out: &str) -> BuildOptions {
    BuildOptions::new()
        .out_dir(dir.join(out))
        .base_dir(dir)
        .cache_dir(cache)
        .offline(true)
        .frozen(true)
}

#[tokio::test]
async fn builds_a_javascript_environment_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());

    let environment = build(&definition(), &options(dir.path(), &cache, "env"))
        .await
        .unwrap();

    assert_eq!(environment.language(), Language::JavaScript);
    let exports: Vec<(&str, &str)> = environment
        .exports()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        exports,
        [
            ("@scope/sub", "modules/@scope__sub.js"),
            ("esm-pkg", "modules/esm-pkg.js"),
            ("esm-pkg/extra", "modules/esm-pkg__extra.js"),
            ("uses-builtins", "modules/uses-builtins.js"),
        ]
    );
    let root = environment.root();
    let esm = std::fs::read_to_string(root.join("modules/esm-pkg.js")).unwrap();
    assert!(esm.contains(r#"target = "browser""#), "{esm}");
    let builtins = std::fs::read_to_string(root.join("modules/uses-builtins.js")).unwrap();
    assert!(builtins.contains(r#"from "node:fs""#), "{builtins}");
    assert!(root.join("eryx-npm.lock").exists());
    assert!(!root.join("modules/bundle.json").exists());

    let manifest = environment.manifest();
    assert_eq!(manifest.builder.tools["bundler"], "rolldown 1.3.2");
    assert_eq!(manifest.inputs.lockfile.as_deref(), Some("eryx-npm.lock"));
    assert_eq!(
        manifest.inputs.packages,
        ["@scope/sub@1", "esm-pkg@1", "uses-builtins@1"]
    );

    // Loading checks the content against the manifest.
    let loaded = Environment::load(root).unwrap();
    assert_eq!(loaded.manifest(), manifest);
    std::fs::write(root.join("modules/esm-pkg.js"), "tampered").unwrap();
    assert!(Environment::load(root).is_err());
}

#[tokio::test]
async fn rebuilding_reproduces_the_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());
    let first = build(&definition(), &options(dir.path(), &cache, "a"))
        .await
        .unwrap();
    let second = build(&definition(), &options(dir.path(), &cache, "b"))
        .await
        .unwrap();
    assert_eq!(first.manifest().content, second.manifest().content);

    // Building over an environment replaces it.
    let again = build(&definition(), &options(dir.path(), &cache, "a"))
        .await
        .unwrap();
    assert_eq!(again.manifest().content, first.manifest().content);
}

#[tokio::test]
async fn bundling_failures_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());
    let definition = EnvironmentDefinition::from_toml(
        r#"
        name = "fixtures"
        language = "javascript"

        [npm]
        packages = ["esm-pkg@1", "uses-builtins@1", "@scope/sub@1"]
        entries = ["esm-pkg/missing"]
        "#,
    )
    .unwrap();

    let err = build(&definition, &options(dir.path(), &cache, "env"))
        .await
        .unwrap_err();
    assert!(matches!(err, EnvironmentError::Build(_)), "{err}");
    assert!(err.to_string().contains("esm-pkg/missing"), "{err}");
    assert!(!dir.path().join("env").exists());
}

#[tokio::test]
async fn offline_without_the_packages_fails_before_bundling() {
    let dir = tempfile::tempdir().unwrap();
    let (_, _) = prepare(dir.path());
    let empty_cache = dir.path().join("empty");
    let err = build(&definition(), &options(dir.path(), &empty_cache, "env"))
        .await
        .unwrap_err();
    assert!(matches!(err, EnvironmentError::Packages(_)), "{err}");
}
