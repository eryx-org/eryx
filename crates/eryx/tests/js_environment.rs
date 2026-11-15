//! Importing an environment's packages from JavaScript (#827).
//!
//! The packages are bundled copies of zod, date-fns and lodash
//! (`tests/fixtures/js-environment`, see its README); each test builds an
//! environment around them in a temporary directory and mounts it.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript,environment --test js_environment
//! ```
#![cfg(all(feature = "embedded", feature = "javascript", feature = "environment"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eryx::environment::{Builder, Environment, Inputs, Language, Manifest};
use eryx::{ErrorKind, Sandbox, SessionExecutor, StateSnapshot};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/js-environment")
}

/// An environment of the fixture packages, in `dir`.
fn environment(dir: &Path) -> Environment {
    let modules = dir.join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    for entry in std::fs::read_dir(fixtures().join("modules")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures().join("bundle.json")).unwrap()).unwrap();
    let mut exports: BTreeMap<String, String> = report["entries"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, file)| (name.clone(), format!("modules/{}", file.as_str().unwrap())))
        .collect();
    // A module whose top-level await never settles, for `require`.
    std::fs::write(
        modules.join("waits.js"),
        "await new Promise(() => {});\nexport const x = 1;\n",
    )
    .unwrap();
    exports.insert("waits".to_string(), "modules/waits.js".to_string());
    let manifest = Manifest::new(
        "fixtures",
        Language::JavaScript,
        Inputs::new(vec![
            "zod@4.6.7".into(),
            "date-fns@4.4.0".into(),
            "lodash@4.18.1".into(),
        ]),
        Builder::new("test"),
        dir,
        "modules",
        exports,
    )
    .unwrap();
    manifest.write(dir).unwrap();
    Environment::load(dir).unwrap()
}

async fn run(sandbox: &eryx::Sandbox, code: &str) -> Result<String, eryx::Error> {
    sandbox.execute(code).await.map(|out| out.stdout_text())
}

#[tokio::test]
async fn imports_packages_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_environment(&env)
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
        // Static imports at the top of a script, in every form.
        import { z } from "zod";
        import * as dates from "date-fns";
        import _, { chunk } from "lodash";
        import { format as formatDate } from 'date-fns' // with a comment

        console.log(z.object({ a: z.string() }).parse({ a: "ok" }).a);
        console.log(formatDate(new Date(0), "yyyy-MM-dd"), typeof dates.addDays);
        console.log(JSON.stringify(chunk([1, 2, 3, 4], 2)), _.uniq([1, 1, 2]).length);
        "#,
    )
    .await
    .unwrap();
    assert_eq!(out, "ok\n1970-01-01 function\n[[1,2],[3,4]] 2\n");

    // `await import()` anywhere, and `require` for CommonJS-style code.
    let out = run(
        &sandbox,
        r#"
        const later = await import("date-fns");
        const lodash = require("lodash");
        const { z } = require("zod");
        console.log(typeof later.format, lodash.chunk([1, 2], 1).length, typeof z.string);
        "#,
    )
    .await
    .unwrap();
    assert_eq!(out, "function 2 function\n");
}

#[tokio::test]
async fn unknown_modules_and_builtins_fail_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_environment(&env)
        .build()
        .unwrap();

    for (code, expected) in [
        (
            "await import('left-pad')",
            "environment provides date-fns, lodash, waits, zod",
        ),
        (
            "import x from 'left-pad'\n",
            "environment provides date-fns, lodash, waits, zod",
        ),
        (
            "require('left-pad')",
            "environment provides date-fns, lodash, waits, zod",
        ),
        (
            "await import('node:fs')",
            "node:fs isn't available in eryx yet",
        ),
        ("require('fs')", "node:fs isn't available in eryx yet"),
        (
            "await import('../eryx-environment.json')",
            "relative imports only work",
        ),
        ("require('waits')", "uses top-level await"),
    ] {
        let err = sandbox.execute(code).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains(expected), "{code}: {message}");
    }

    // Without an environment.
    let bare = Sandbox::embedded().javascript().build().unwrap();
    let err = bare.execute("await import('zod')").await.unwrap_err();
    assert!(err.to_string().contains("has no environment"), "{err}");
}

#[tokio::test]
async fn sessions_keep_and_reimport_modules() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let executor = Sandbox::embedded()
        .javascript()
        .with_environment(&env)
        .build()
        .unwrap()
        .executor();
    let mut session = SessionExecutor::new(executor, &[]).await.unwrap();
    for _ in 0..2 {
        // The same import again in a later execute is fine.
        let out = session
            .execute("import { chunk } from 'lodash'\nconsole.log(chunk([1, 2, 3], 2).length)")
            .run()
            .await
            .unwrap();
        assert_eq!(out.stdout, b"2\n");
    }
    let out = session
        .execute("console.log(chunk([1], 1).length)")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout, b"1\n");
}

async fn new_session(env: &Environment) -> SessionExecutor {
    let executor = Sandbox::embedded()
        .javascript()
        .with_environment(env)
        .build()
        .unwrap()
        .executor();
    SessionExecutor::new(executor, &[]).await.unwrap()
}

async fn stdout(session: &mut SessionExecutor, code: &str) -> String {
    session.execute(code).run().await.unwrap().stdout_text()
}

/// `snapshot` with every `from` replaced by `to`, of the same length so the
/// encoding stays readable, and the encoding's checksum set to the value
/// QuickJS accepts for any data.
fn patched(snapshot: &StateSnapshot, from: &str, to: &str) -> StateSnapshot {
    assert_eq!(from.len(), to.len());
    let mut bytes = snapshot.to_bytes();
    let mut found = false;
    for i in 0..=bytes.len() - from.len() {
        if &bytes[i..i + from.len()] == from.as_bytes() {
            bytes[i..i + from.len()].copy_from_slice(to.as_bytes());
            found = true;
        }
    }
    assert!(found, "{from} isn't in the snapshot");
    let data = bytes.len() - snapshot.data().len();
    let encoding = data + snapshot.data().iter().position(|&b| b == b'\n').unwrap() + 1;
    // A version byte, then the checksum.
    bytes[encoding + 1..encoding + 5].fill(0xff);
    StateSnapshot::from_bytes(&bytes).unwrap()
}

#[tokio::test]
async fn snapshots_import_again_on_restore() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let mut session = new_session(&env).await;
    stdout(
        &mut session,
        "import { chunk, uniq as unique } from 'lodash'\nimport * as dates from 'date-fns'\nimport _ from 'lodash'\nconst n = 5",
    )
    .await;
    // Reassigned since the import, in a later execute or the same one: the
    // new value is kept instead.
    stdout(&mut session, "unique = 42").await;
    stdout(
        &mut session,
        "import { last as final } from 'lodash'\nfinal = 7",
    )
    .await;
    let snapshot = session.snapshot_state().await.unwrap();
    // Imported functions and namespaces come back by importing, not as values.
    assert_eq!(snapshot.skipped(), &[] as &[String]);

    let mut restored = new_session(&env).await;
    restored.restore_state(&snapshot).await.unwrap();
    assert_eq!(
        stdout(
            &mut restored,
            "console.log(chunk([1, 2, 3], 2).length, typeof dates.format, _.last([1, 2]), unique, final, n)"
        )
        .await,
        "2 function 2 42 7 5\n"
    );
    // ... and a snapshot of the restored session still has them.
    let again = restored.snapshot_state().await.unwrap();
    let mut restored = new_session(&env).await;
    restored.restore_state(&again).await.unwrap();
    assert_eq!(
        stdout(
            &mut restored,
            "console.log(chunk([1], 1).length, unique, final)"
        )
        .await,
        "1 42 7\n"
    );
}

#[tokio::test]
async fn crafted_or_failing_imports_fail_the_restore() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let mut session = new_session(&env).await;
    stdout(
        &mut session,
        "import { chunk as myLocal } from 'lodash'\nconst n = 5",
    )
    .await;
    let snapshot = session.snapshot_state().await.unwrap();

    for (snapshot, expected) in [
        // A local name that isn't an identifier would be source code.
        (
            patched(&snapshot, "myLocal", "x;y()//"),
            "malformed session state",
        ),
        (patched(&snapshot, "lodash", "nopkg_"), "nopkg_"),
    ] {
        let mut restored = new_session(&env).await;
        let err = restored.restore_state(&snapshot).await.unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
        // Nothing of the snapshot is left.
        assert_eq!(
            stdout(&mut restored, "console.log(typeof n)").await,
            "undefined\n"
        );
    }
}

#[tokio::test]
async fn layout_1_snapshots_still_restore() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let mut session = new_session(&env).await;
    stdout(&mut session, "const n = 5").await;
    // Layout 1 is layout 2 without imports.
    let snapshot = patched(
        &session.snapshot_state().await.unwrap(),
        "eryx-js-state/2 ",
        "eryx-js-state/1 ",
    );
    let mut restored = new_session(&env).await;
    restored.restore_state(&snapshot).await.unwrap();
    assert_eq!(stdout(&mut restored, "console.log(n)").await, "5\n");
}

#[tokio::test]
async fn the_environment_is_checked_when_the_sandbox_is_built() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());

    // Another language.
    let err = Sandbox::embedded()
        .with_environment(&env)
        .build()
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Initialization(m) if m.contains("javascript environment")),
        "{err}"
    );

    // Changed since it was loaded.
    let builder = Sandbox::embedded().javascript().with_environment(&env);
    std::fs::write(dir.path().join("modules/zod.js"), "export const z = 1;").unwrap();
    let err = builder.build().unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Initialization(m) if m.contains("integrity")),
        "{err}"
    );
}
