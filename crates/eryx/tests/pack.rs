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
    assert_no_canary(dir.path(), &app.join("service-account.json"));
}

/// No regular file under `dir` but `source` holds the canary.
fn assert_no_canary(dir: &Path, source: &Path) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            // Only files' own contents: the fixtures' links lead back to the
            // source.
            let kind = path.symlink_metadata().unwrap().file_type();
            if kind.is_dir() {
                pending.push(path);
            } else if path != source && kind.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !bytes.windows(6).any(|w| w == b"CANARY"),
                    "{} holds the canary",
                    path.display()
                );
            }
        }
    }
}

/// A linked workspace member is visible whole, but nothing beside it: its
/// import of a file at the workspace root fails the pack.
#[tokio::test]
async fn a_workspace_member_cant_import_its_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let member = repo.join("packages/w");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::write(repo.join("secret.txt"), "CANARY").unwrap();
    std::fs::write(
        member.join("package.json"),
        r#"{"name": "w", "version": "1.0.0", "main": "index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        member.join("index.js"),
        "import text from \"../../secret.txt\";\nexport default text;\n",
    )
    .unwrap();
    let app = repo.join("apps/web");
    std::fs::create_dir_all(app.join("node_modules")).unwrap();
    symlink("../../../packages/w", app.join("node_modules/w")).unwrap();

    let out = dir.path().join("env");
    let err = pack(&app, &out, &options("ws").entry("w").root(&repo))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("secret.txt"), "{err}");
    assert!(!out.exists(), "nothing was written");
    assert_no_canary(dir.path(), &repo.join("secret.txt"));
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

/// A pnpm workspace member: its package links into the root's `.pnpm`
/// store, where its own dependencies are siblings. They resolve, as for
/// Node, from the package's real path.
#[tokio::test]
async fn packs_a_pnpm_workspace_member_with_transitive_deps() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let store = repo.join("node_modules/.pnpm");
    copy_package("esm-pkg", &store.join("esm-pkg@1.0.0/node_modules/esm-pkg"));
    copy_package("cjs-pkg", &store.join("cjs-pkg@1.0.0/node_modules/cjs-pkg"));
    symlink(
        "../../cjs-pkg@1.0.0/node_modules/cjs-pkg",
        store.join("esm-pkg@1.0.0/node_modules/cjs-pkg"),
    )
    .unwrap();
    let app = repo.join("apps/web");
    std::fs::create_dir_all(app.join("node_modules")).unwrap();
    symlink(
        "../../../node_modules/.pnpm/esm-pkg@1.0.0/node_modules/esm-pkg",
        app.join("node_modules/esm-pkg"),
    )
    .unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"dependencies": {"esm-pkg": "^1"}}"#,
    )
    .unwrap();

    let environment = pack(
        &app,
        &dir.path().join("env"),
        &options("pnpm-ws").root(&repo),
    )
    .await
    .unwrap();
    assert!(
        environment.manifest().warnings.is_empty(),
        "{:?}",
        environment.manifest().warnings
    );
    imports(
        &environment,
        r#"import { doubled } from "esm-pkg"; console.log(doubled);"#,
        "42\n",
    )
    .await;
}

/// An import the installed packages don't provide fails the pack, rather
/// than leaving an environment that fails when imported.
#[tokio::test]
async fn unresolved_imports_fail_the_pack() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    copy_package("needs-missing", &app.join("node_modules/needs-missing"));
    let out = dir.path().join("env");
    let err = pack(&app, &out, &options("missing").entry("needs-missing"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("can't be resolved") && err.contains("not-installed"),
        "{err}"
    );
    assert!(!out.exists());
}

/// A node_modules that's a link is followed like a package link: inside the
/// root it packs, and to a directory holding the tree it's refused.
#[tokio::test]
async fn a_linked_node_modules_is_checked_and_followed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    copy_package("cjs-pkg", &repo.join("store/node_modules/cjs-pkg"));
    let app = repo.join("app");
    std::fs::create_dir_all(&app).unwrap();
    symlink("../store/node_modules", app.join("node_modules")).unwrap();
    let options = options("linked").entry("cjs-pkg");
    let environment = pack(&app, &dir.path().join("env"), &options.clone().root(&repo))
        .await
        .unwrap();
    imports(
        &environment,
        r#"import { helper } from "cjs-pkg"; console.log(helper(1));"#,
        "2\n",
    )
    .await;

    std::fs::remove_file(app.join("node_modules")).unwrap();
    symlink("..", app.join("node_modules")).unwrap();
    let err = pack(&app, &dir.path().join("env"), &options.root(&repo))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("holding the tree"), "{err}");
}

/// A symlink inside a package keeps its exact target in the bundler's
/// mirror, so one to the app's files resolves to nothing there: importing
/// through it fails the pack rather than reading the file.
#[tokio::test]
async fn a_package_symlink_to_the_apps_files_doesnt_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let evil = app.join("node_modules/evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(app.join("service-account.json"), r#"{"key": "CANARY"}"#).unwrap();
    symlink("../../service-account.json", evil.join("data.json")).unwrap();
    std::fs::write(
        evil.join("package.json"),
        r#"{"name": "evil", "version": "1.0.0", "main": "index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        evil.join("index.js"),
        "import key from \"./data.json\";\nexport default key;\n",
    )
    .unwrap();
    let out = dir.path().join("env");
    let err = pack(&app, &out, &options("evil-link").entry("evil"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("data.json"), "{err}");
    assert!(!out.exists(), "nothing was written");
    assert_no_canary(dir.path(), &app.join("service-account.json"));
    // And the mirror went with the failure.
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".eryx-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Links in a package to above the mirror, or absolute ones, resolve to
/// nothing the bundler can read: importing through them fails the pack.
#[tokio::test]
async fn a_package_symlink_out_of_the_mirror_or_absolute_doesnt_resolve() {
    let up = "../".repeat(14);
    for (name, target) in [
        ("above", format!("{up}etc/hostname")),
        ("absolute", "/etc/hostname".to_string()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let evil = app.join("node_modules/evil");
        std::fs::create_dir_all(&evil).unwrap();
        symlink(&target, evil.join("data.txt")).unwrap();
        std::fs::write(
            evil.join("package.json"),
            r#"{"name": "evil", "version": "1.0.0", "main": "index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            evil.join("index.js"),
            "import text from \"./data.txt\";\nexport default text;\n",
        )
        .unwrap();
        let out = dir.path().join("env");
        let result = pack(&app, &out, &options(name).entry("evil")).await;
        assert!(result.is_err(), "{name}: packed");
        assert!(!out.exists(), "{name}: nothing was written");
    }
}

/// A package with nothing to import at its main path, for the tests below.
fn package(dir: &Path, json: &str, index: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), json).unwrap();
    std::fs::write(dir.join("index.js"), index).unwrap();
}

/// Optional dependencies (`ws`'s `bufferutil`, `node-fetch`'s `encoding`)
/// aren't installed by `npm ci --omit=dev`.
/// - A `require` of one in a try/catch, as those packages do it, is left
///   external without a warning: the pack succeeds, and the code copes.
/// - A static import of one the package declares optional (an optional peer
///   or an `optionalDependencies` entry) stays external with a warning.
/// - Any other unresolved import fails the pack, unless allowed.
#[tokio::test]
async fn optional_imports_may_stay_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    let nm = app.join("node_modules");
    package(
        &nm.join("ws-like"),
        r#"{"name": "ws-like", "version": "1.0.0", "main": "index.js",
            "peerDependencies": {"bufferutil": "^4"},
            "peerDependenciesMeta": {"bufferutil": {"optional": true}}}"#,
        "let found = null;\ntry { found = require(\"bufferutil\"); } catch {}\n\
         module.exports = { found: found !== null };\n",
    );
    package(
        &nm.join("peer-import"),
        r#"{"name": "peer-import", "version": "1.0.0", "main": "index.js", "type": "module",
            "peerDependencies": {"bufferutil": "^4"},
            "peerDependenciesMeta": {"bufferutil": {"optional": true}}}"#,
        "import * as b from \"bufferutil\";\nexport const has = () => b;\n",
    );
    package(
        &nm.join("optional-import"),
        r#"{"name": "optional-import", "version": "1.0.0", "main": "index.js", "type": "module",
            "optionalDependencies": {"encoding": "^0.1"}}"#,
        "import * as e from \"encoding\";\nexport const has = () => e;\n",
    );
    package(
        &nm.join("undeclared"),
        r#"{"name": "undeclared", "version": "1.0.0", "main": "index.js", "type": "module"}"#,
        "import * as l from \"left-pad\";\nexport const has = () => l;\n",
    );

    let environment = pack(
        &app,
        &dir.path().join("env"),
        &options("try").entry("ws-like"),
    )
    .await
    .unwrap();
    assert!(
        environment.manifest().warnings.is_empty(),
        "{:?}",
        environment.manifest().warnings
    );
    imports(
        &environment,
        r#"import ws from "ws-like"; console.log(ws.found);"#,
        "false\n",
    )
    .await;

    let declared = options("declared")
        .entry("peer-import")
        .entry("optional-import");
    let environment = pack(&app, &dir.path().join("env2"), &declared)
        .await
        .unwrap();
    let warnings = &environment.manifest().warnings;
    for name in ["bufferutil", "encoding"] {
        assert!(
            warnings.iter().any(|w| w.contains(&format!("'{name}'"))),
            "{name}: {warnings:?}"
        );
    }

    let err = pack(
        &app,
        &dir.path().join("env3"),
        &options("undeclared").entry("undeclared"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("left-pad") && err.contains("--allow-unresolved"),
        "{err}"
    );
    let allowed = options("allowed")
        .entry("undeclared")
        .allow_unresolved("left-pad");
    let environment = pack(&app, &dir.path().join("env4"), &allowed)
        .await
        .unwrap();
    assert!(
        environment
            .manifest()
            .warnings
            .iter()
            .any(|w| w.contains("'left-pad'"))
    );
}
