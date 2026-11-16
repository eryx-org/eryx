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

const PACKAGES: [&str; 6] = [
    "@scope/sub",
    "cjs-pkg",
    "esm-pkg",
    "events",
    "needs-missing",
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

/// A cache holding the fixtures' tarballs, and a lockfile pinning them,
/// resolved from the specs [`definition`] asks for.
fn prepare(dir: &Path) -> (PathBuf, PathBuf) {
    prepare_for(
        dir,
        "fixtures",
        &["@scope/sub@1", "esm-pkg@1", "uses-builtins@1"],
    )
}

fn prepare_for(dir: &Path, name: &str, requested: &[&str]) -> (PathBuf, PathBuf) {
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
        requested: requested.iter().map(ToString::to_string).collect(),
        packages,
    };
    let path = dir.join(format!("{name}.lock"));
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

#[tokio::test]
async fn warnings_are_recorded_in_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare_for(dir.path(), "warns", &["needs-missing@1"]);
    let definition = EnvironmentDefinition::from_toml(
        r#"
        name = "warns"
        language = "javascript"

        [npm]
        packages = ["needs-missing@1"]
        "#,
    )
    .unwrap();

    let environment = build(&definition, &options(dir.path(), &cache, "env"))
        .await
        .unwrap();

    let warnings = &environment.manifest().warnings;
    assert!(
        warnings.iter().any(|w| w.contains("not-installed")),
        "{warnings:?}"
    );
    assert_eq!(
        Environment::load(environment.root())
            .unwrap()
            .manifest()
            .warnings,
        *warnings
    );
}

#[tokio::test]
async fn concurrent_builds_of_one_environment_all_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());
    let definition = definition();

    // Into the same explicit directory: each build replaces the last.
    let explicit = dir.path().join("shared");
    let builds = (0..4).map(|_| {
        let options = BuildOptions::new()
            .out_dir(&explicit)
            .base_dir(dir.path())
            .cache_dir(&cache)
            .offline(true)
            .frozen(true);
        let definition = definition.clone();
        tokio::spawn(async move {
            build(&definition, &options)
                .await
                .map(|e| e.manifest().clone())
        })
    });
    let manifests: Vec<_> = futures::future::join_all(builds).await;
    let hash = manifests[0]
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .content
        .hash
        .clone();
    for manifest in &manifests {
        assert_eq!(
            manifest.as_ref().unwrap().as_ref().unwrap().content.hash,
            hash
        );
    }
    Environment::load_expecting(&explicit, &hash).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Into the cache: one wins, the others use it.
    let builds = (0..4).map(|_| {
        let options = BuildOptions::new()
            .base_dir(dir.path())
            .cache_dir(&cache)
            .offline(true)
            .frozen(true);
        let definition = definition.clone();
        tokio::spawn(async move {
            build(&definition, &options)
                .await
                .map(|e| e.root().to_path_buf())
        })
    });
    let roots: Vec<_> = futures::future::join_all(builds)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();
    assert!(roots.iter().all(|r| *r == roots[0]));
    Environment::load_expecting(&roots[0], &hash).unwrap();
}

#[tokio::test]
async fn fills_an_existing_empty_directory_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());
    let volume = dir.path().join("volume");
    std::fs::create_dir(&volume).unwrap();

    let environment = build(&definition(), &options(dir.path(), &cache, "volume"))
        .await
        .unwrap();

    assert_eq!(environment.root(), volume);
    let mut names: Vec<_> = std::fs::read_dir(&volume)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["eryx-environment.json", "eryx-npm.lock", "modules"]);
}

#[tokio::test]
async fn load_expecting_pins_the_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (cache, _) = prepare(dir.path());
    let environment = build(&definition(), &options(dir.path(), &cache, "env"))
        .await
        .unwrap();
    let hash = &environment.manifest().content.hash;
    Environment::load_expecting(environment.root(), hash).unwrap();
    let err = Environment::load_expecting(environment.root(), "sha256-0").unwrap_err();
    assert!(err.to_string().contains("sha256-0"), "{err}");
}

/// Without a base directory, the lockfile lives in the cache directory, not
/// the process's current directory; a definition built in code builds as a
/// parsed one does.
#[tokio::test]
async fn the_default_lockfile_is_in_the_cache_dir() {
    use eryx::environment::NpmSection;

    let dir = tempfile::tempdir().unwrap();
    let (cache, lockfile) = prepare(dir.path());
    std::fs::create_dir_all(cache.join("lockfiles")).unwrap();
    std::fs::rename(&lockfile, cache.join("lockfiles/fixtures.lock")).unwrap();

    let definition =
        EnvironmentDefinition::new("fixtures", eryx::environment::Language::JavaScript)
            .unwrap()
            .with_npm(
                NpmSection::new(["esm-pkg@1", "uses-builtins@1", "@scope/sub@1"])
                    .entries(["esm-pkg/extra"]),
            );
    let options = BuildOptions::new()
        .out_dir(dir.path().join("out"))
        .cache_dir(&cache)
        .offline(true)
        .frozen(true);
    let environment = build(&definition, &options).await.unwrap();
    assert!(environment.exports().contains_key("esm-pkg/extra"));
    assert!(!Path::new("fixtures.lock").exists());
}

/// A definition's `lockfile` in a subdirectory gets that directory created in
/// the cache, so resolving can write it there.
#[tokio::test]
async fn a_default_lockfile_in_a_subdirectory_gets_its_directory() {
    use eryx::environment::NpmSection;

    let dir = tempfile::tempdir().unwrap();
    let (cache, lockfile) = prepare(dir.path());
    let nested = cache.join("lockfiles/locks");
    assert!(!nested.exists());
    let definition =
        EnvironmentDefinition::new("fixtures", eryx::environment::Language::JavaScript)
            .unwrap()
            .with_npm(
                NpmSection::new(["esm-pkg@1", "uses-builtins@1", "@scope/sub@1"])
                    .lockfile("locks/fixtures.lock"),
            );
    let options = BuildOptions::new()
        .out_dir(dir.path().join("out"))
        .cache_dir(&cache)
        .offline(true)
        .frozen(true);

    // Without the lockfile there, a frozen build fails, but its directory is made.
    assert!(build(&definition, &options).await.is_err());
    assert!(nested.is_dir());

    // With it there, the build reads it.
    std::fs::rename(&lockfile, nested.join("fixtures.lock")).unwrap();
    build(&definition, &options).await.unwrap();
}
