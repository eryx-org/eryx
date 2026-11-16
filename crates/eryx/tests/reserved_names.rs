//! Callback names a guest already uses are refused at build (#962). The
//! host keeps the lists (crates/eryx/src/reserved/); these check them
//! against fresh JavaScript and Python guests (the shell guest checks its
//! own, in its crate's tests).
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{CallbackError, DynamicCallback, Sandbox};
use serde_json::json;

fn list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

fn callback(name: &str) -> DynamicCallback {
    DynamicCallback::builder(name, "test", |_| {
        Box::pin(async { Ok::<_, CallbackError>(json!(null)) })
    })
    .build()
}

/// Every global of a fresh JavaScript guest, on the global object or its
/// prototype chain, is in the list, and the list has nothing else but the
/// host preamble's `process`.
#[tokio::test]
async fn javascript_list_matches_the_guest() {
    let sandbox = Sandbox::embedded().javascript().build().unwrap();
    let out = sandbox
        .execute(
            "const names = new Set();\n\
             for (let o = globalThis; o; o = Object.getPrototypeOf(o)) \
             for (const n of Object.getOwnPropertyNames(o)) names.add(n);\n\
             console.log([...names].sort().join('\\n'))",
        )
        .await
        .unwrap();
    let mut guest: Vec<String> = out.stdout_text().lines().map(String::from).collect();
    guest.push("process".to_string());
    guest.sort();
    assert_eq!(
        list(include_str!("../src/reserved/javascript.txt")),
        guest,
        "update crates/eryx/src/reserved/javascript.txt (and run `mise run update-reserved-names`)"
    );
}

/// The Python list is the guest's builtins plus its preamble's names (which
/// a unit test checks against python.rs).
#[tokio::test]
async fn python_list_has_the_guest_builtins() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox
        .execute("print('\\n'.join(sorted(dir(__builtins__))))")
        .await
        .unwrap();
    let reserved = list(include_str!("../src/reserved/python.txt"));
    for name in out.stdout_text().lines() {
        assert!(reserved.iter().any(|r| r == name), "{name} is missing");
    }
}

#[tokio::test]
async fn javascript_refuses_globals_at_build() {
    for name in ["fetch", "crypto.subtle", "setTimeout", "toString", "URL.*"] {
        let err = Sandbox::embedded()
            .javascript()
            .with_callback(callback(name))
            .build()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("collides with the JavaScript global"),
            "{name}: {err}"
        );
    }
    Sandbox::embedded()
        .javascript()
        .with_callback(callback("fetch_user"))
        .with_callback(callback("mcp.fetch.*"))
        .build()
        .unwrap();
}

/// Python refuses a builtin's name, and a name the guest defines itself,
/// as the other guests do.
#[test]
fn python_refuses_its_builtins() {
    for name in ["print", "json.dumps", "invoke"] {
        let err = Sandbox::embedded()
            .with_callback(callback(name))
            .build()
            .unwrap_err();
        assert!(
            err.to_string().contains("collides with the Python builtin"),
            "{name}: {err}"
        );
    }
    Sandbox::embedded()
        .with_callback(callback("print_report"))
        .build()
        .unwrap();
}
