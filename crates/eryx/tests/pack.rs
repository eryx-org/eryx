//! Packing environments from installed `node_modules` trees
//! (`eryx::environment::pack`), in the layouts npm and pnpm leave.
//!
//! The packages are the bundler's fixtures (`crates/eryx-bundler/tests/fixture`),
//! copied into each layout; the links are made here, so none are committed.
//!
//! ```sh
//! mise run precompile-bundler
//! cargo nextest run -p eryx --features packages --test pack
//! ```
#![cfg(all(feature = "packages", unix))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use eryx::environment::{Environment, EnvironmentError, Inputs, Language, PackOptions, pack};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../eryx-bundler/tests/fixture/node_modules")
        .join(name)
}

/// Copy the fixture package `name` to `dest`.
fn copy_package(name: &str, dest: &Path) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    copy(&fixture(name), dest);
}

fn options(name: &str) -> PackOptions {
    PackOptions::new(name, Language::JavaScript)
}

/// Run `code` in a JavaScript sandbox with `environment`, and check it
/// prints `expected`. Needs the embedded JavaScript runtime (`embedded` and
/// `javascript`, as CI's all-features run has); without it only the packing
/// is checked.
async fn imports(environment: &Environment, code: &str, expected: &str) {
    #[cfg(all(feature = "embedded", feature = "javascript"))]
    {
        let sandbox = eryx::Sandbox::embedded()
            .javascript()
            .with_environment(environment)
            .build()
            .unwrap();
        assert_eq!(sandbox.execute(code).await.unwrap().stdout_text(), expected);
    }
    #[cfg(not(all(feature = "embedded", feature = "javascript")))]
    let _ = (environment, code, expected);
}

/// npm's layout: real directories, a `.bin` link and `.package-lock.json`.
/// Entries default to `package.json`'s dependencies, the inputs record what's
/// installed, and packing the same tree again gives the same hash.
#[tokio::test]
async fn packs_an_npm_tree() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let nm = app.join("node_modules");
    copy_package("esm-pkg", &nm.join("esm-pkg"));
    copy_package("cjs-pkg", &nm.join("cjs-pkg"));
    std::fs::create_dir_all(nm.join(".bin")).unwrap();
    symlink("/usr/bin/true", nm.join(".bin/tool")).unwrap();
    symlink("../nowhere", nm.join("dangling")).unwrap();
    std::fs::write(nm.join(".package-lock.json"), "{}").unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies": {"esm-pkg": "^1", "cjs-pkg": "^1"}}"#,
    )
    .unwrap();
    std::fs::write(app.join("package-lock.json"), r#"{"lockfileVersion": 3}"#).unwrap();

    let out = dir.path().join("env");
    let options = options("npm").lockfile(app.join("package-lock.json"));
    let environment = pack(&app, &out, &options).await.unwrap();
    let manifest = environment.manifest();
    assert_eq!(
        environment.exports().keys().collect::<Vec<_>>(),
        ["cjs-pkg", "esm-pkg"]
    );
    assert_eq!(manifest.inputs.packages, ["cjs-pkg@1.0.0", "esm-pkg@1.0.0"]);
    assert_eq!(
        manifest.inputs.lockfile.as_deref(),
        Some("package-lock.json")
    );
    assert!(out.join("package-lock.json").is_file());
    assert!(
        manifest
            .inputs
            .lockfile_hash
            .as_deref()
            .is_some_and(|h| h.starts_with("sha256-"))
    );
    imports(
        &environment,
        r#"import { doubled, target } from "esm-pkg"; console.log(target, doubled);"#,
        "browser 42\n",
    )
    .await;

    let again = pack(&app, &dir.path().join("env2"), &options)
        .await
        .unwrap();
    assert_eq!(again.manifest().content.hash, manifest.content.hash);
}

/// pnpm's isolated layout: top-level links into `.pnpm`, where each package's
/// dependencies are links to their siblings.
#[tokio::test]
async fn packs_a_pnpm_tree() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let store = app.join("node_modules/.pnpm");
    copy_package("esm-pkg", &store.join("esm-pkg@1.0.0/node_modules/esm-pkg"));
    copy_package("cjs-pkg", &store.join("cjs-pkg@1.0.0/node_modules/cjs-pkg"));
    symlink(
        "../../cjs-pkg@1.0.0/node_modules/cjs-pkg",
        store.join("esm-pkg@1.0.0/node_modules/cjs-pkg"),
    )
    .unwrap();
    symlink(
        ".pnpm/esm-pkg@1.0.0/node_modules/esm-pkg",
        app.join("node_modules/esm-pkg"),
    )
    .unwrap();

    let environment = pack(
        &app,
        &dir.path().join("env"),
        &options("pnpm").entry("esm-pkg"),
    )
    .await
    .unwrap();
    assert_eq!(
        environment.manifest().inputs.packages,
        ["esm-pkg@1.0.0"],
        "the version is read through the link"
    );
    imports(
        &environment,
        r#"import { doubled } from "esm-pkg"; console.log(doubled);"#,
        "42\n",
    )
    .await;
}

/// A workspace package (or a `file:` dependency) linked from outside the
/// tree: refused with the tree as the root, packed with the repo's.
#[tokio::test]
async fn links_outside_the_tree_need_a_root_that_holds_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    copy_package("cjs-pkg", &repo.join("packages/cjs-pkg"));
    let app = repo.join("apps/web");
    std::fs::create_dir_all(app.join("node_modules")).unwrap();
    symlink(
        "../../../packages/cjs-pkg",
        app.join("node_modules/cjs-pkg"),
    )
    .unwrap();
    let options = options("workspace").entry("cjs-pkg");

    let err = pack(&app, &dir.path().join("env"), &options)
        .await
        .unwrap_err();
    assert!(matches!(err, EnvironmentError::Build(_)), "{err}");
    let message = err.to_string();
    assert!(
        message.contains("node_modules/cjs-pkg")
            && message.contains("leads outside the root")
            && message.contains("--root"),
        "{message}"
    );

    let environment = pack(&app, &dir.path().join("env"), &options.root(&repo))
        .await
        .unwrap();
    imports(
        &environment,
        r#"import { helper } from "cjs-pkg"; console.log(helper(4));"#,
        "8\n",
    )
    .await;
}

/// An absolute package link is refused even when it points inside the tree:
/// the bundler can't follow it.
#[tokio::test]
async fn absolute_links_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    copy_package("cjs-pkg", &app.join("vendor/cjs-pkg"));
    std::fs::create_dir_all(app.join("node_modules")).unwrap();
    symlink(app.join("vendor/cjs-pkg"), app.join("node_modules/cjs-pkg")).unwrap();
    let err = pack(
        &app,
        &dir.path().join("env"),
        &options("abs").entry("cjs-pkg"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("absolute symlink"), "{err}");
}

/// A tree that isn't installed, or exports nothing, says so before bundling,
/// and given inputs replace the derived ones.
#[tokio::test]
async fn bad_trees_fail_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let err = pack(dir.path(), &dir.path().join("env"), &options("none"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no node_modules in") && err.contains("npm ci"),
        "{err}"
    );

    let app = dir.path().join("app");
    copy_package("cjs-pkg", &app.join("node_modules/cjs-pkg"));
    let err = pack(&app, &dir.path().join("env"), &options("empty"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing to export"), "{err}");

    let environment = pack(
        &app,
        &dir.path().join("env"),
        &options("given")
            .entry("cjs-pkg")
            .inputs(Inputs::new(vec!["cjs-pkg@^1".to_string()])),
    )
    .await
    .unwrap();
    assert_eq!(environment.manifest().inputs.packages, ["cjs-pkg@^1"]);
}

/// The bundler sees only node_modules and the packages linked from them, so
/// a package can't pull the app's own files into the environment: a
/// relative import of `../../service-account.json` fails the pack instead.
#[tokio::test]
async fn packages_cant_import_the_apps_files() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let evil = app.join("node_modules/evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(app.join("service-account.json"), r#"{"key": "CANARY"}"#).unwrap();
    std::fs::write(
        evil.join("package.json"),
        r#"{"name": "evil", "version": "1.0.0", "main": "index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        evil.join("index.js"),
        "import key from \"../../service-account.json\";\nexport default key;\n",
    )
    .unwrap();
    let out = dir.path().join("env");
    let err = pack(&app, &out, &options("evil").entry("evil"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("service-account.json"), "{err}");
    assert!(!out.exists(), "nothing was written");
}

/// A workspace member whose dependencies are hoisted to the repo's
/// node_modules (it has none of its own) packs with the repo as its root,
/// with the root's files still out of reach.
#[tokio::test]
async fn packs_a_hoisted_workspace_member() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    copy_package("esm-pkg", &repo.join("node_modules/esm-pkg"));
    copy_package("cjs-pkg", &repo.join("node_modules/cjs-pkg"));
    let app = repo.join("apps/web");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies": {"esm-pkg": "^1"}}"#,
    )
    .unwrap();

    let err = pack(&app, &dir.path().join("env"), &options("web"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no node_modules in"), "{err}");

    let environment = pack(&app, &dir.path().join("env"), &options("web").root(&repo))
        .await
        .unwrap();
    assert_eq!(environment.manifest().inputs.packages, ["esm-pkg@1.0.0"]);
    imports(
        &environment,
        r#"import { doubled } from "esm-pkg"; console.log(doubled);"#,
        "42\n",
    )
    .await;
}

/// With entries defaulted from package.json, a dependency with nothing to
/// import (types-only, CLI-only) is skipped with a warning; named, it isn't.
#[tokio::test]
async fn defaulted_entries_skip_packages_with_nothing_to_import() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    copy_package("cjs-pkg", &app.join("node_modules/cjs-pkg"));
    let types = app.join("node_modules/@types/cjs-pkg");
    std::fs::create_dir_all(&types).unwrap();
    std::fs::write(types.join("package.json"), r#"{"types": "index.d.ts"}"#).unwrap();
    std::fs::write(types.join("index.d.ts"), "export {};").unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies": {"cjs-pkg": "^1", "@types/cjs-pkg": "^1"}}"#,
    )
    .unwrap();

    let environment = pack(&app, &dir.path().join("env"), &options("skip"))
        .await
        .unwrap();
    assert_eq!(
        environment.exports().keys().collect::<Vec<_>>(),
        ["cjs-pkg"]
    );
    let warnings = &environment.manifest().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("@types/cjs-pkg isn't exported")),
        "{warnings:?}"
    );

    let named = options("named").entry("@types/cjs-pkg");
    assert!(pack(&app, &dir.path().join("env2"), &named).await.is_err());
}
