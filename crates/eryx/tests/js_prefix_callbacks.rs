//! Prefix callbacks (`mcp.svc.*`) in the JavaScript guest (#939): every
//! dotted name below the prefix reaches the one callback, with the full name
//! the script called, as in the Python guest (`tests/prefix_callbacks.rs`).
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_prefix_callbacks
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eryx::{DynamicCallback, InProcessSession, PrefixCallback, Sandbox, Session};
use serde_json::json;

/// A prefix callback that returns `"{tag}:{name}"` plus the keyword and
/// positional arguments it got.
fn tagged_prefix(prefix: &str, tag: &'static str) -> PrefixCallback {
    PrefixCallback::new(prefix, format!("{tag} tools"), move |name, args, pos| {
        Box::pin(
            async move { Ok(json!({ "hit": format!("{tag}:{name}"), "args": args, "pos": pos })) },
        )
    })
}

/// An ordinary callback returning `"exact:{name}"` plus its arguments, with
/// one declared parameter `q` so positional arguments bind.
fn exact(name: &str) -> DynamicCallback {
    let hit = format!("exact:{name}");
    DynamicCallback::builder(name, "An exact callback", move |args| {
        let hit = hit.clone();
        Box::pin(async move { Ok(json!({ "hit": hit, "args": args })) })
    })
    .param("q", "string", "A query", false)
    .build()
}

fn sandbox() -> Sandbox {
    Sandbox::embedded()
        .javascript()
        .with_callback(tagged_prefix("mcp", "outer"))
        .with_callback(tagged_prefix("mcp.svc", "svc"))
        .with_callback(exact("mcp.svc.known"))
        .build()
        .unwrap()
}

async fn run(sandbox: &Sandbox, code: &str) -> Result<String, eryx::Error> {
    sandbox.execute(code).await.map(|out| out.stdout_text())
}

/// Names of any depth below a prefix reach it with their full name; one
/// plain object is keyword arguments, anything else positional.
#[tokio::test]
async fn names_below_a_prefix_reach_it() {
    let out = run(
        &sandbox(),
        r"
        const show = (r) => console.log(r.hit, JSON.stringify(r.args), JSON.stringify(r.pos));
        show(await mcp.svc.tool({ a: 1 }));
        show(await mcp.svc.nested.tool(1, 2));
        show(await mcp.svc['dashed-name']());
        // The longest prefix wins; the outer one gets the rest.
        show(await mcp.github.issues({ state: 'open' }));
        show(await invoke('mcp.svc.direct', 'x'));
        ",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "svc:mcp.svc.tool {\"a\":1} []\n\
         svc:mcp.svc.nested.tool {} [1,2]\n\
         svc:mcp.svc.dashed-name {} []\n\
         outer:mcp.github.issues {\"state\":\"open\"} []\n\
         svc:mcp.svc.direct {} [\"x\"]\n"
    );
}

/// An exact callback below a prefix wins and keeps its schema, so
/// positional arguments bind to its parameters.
#[tokio::test]
async fn exact_callbacks_win_and_keep_their_schema() {
    let out = run(
        &sandbox(),
        r"
        const known = (r) => console.log(r.hit, JSON.stringify(r.args));
        known(await mcp.svc.known({ q: 'x' }));
        known(await mcp.svc.known('pos'));
        console.log(listCallbacks().map((cb) => cb.name).sort().join(' '));
        ",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "exact:mcp.svc.known {\"q\":\"x\"}\n\
         exact:mcp.svc.known {\"q\":\"pos\"}\n\
         mcp.* mcp.svc.* mcp.svc.known\n"
    );
}

/// Outside any prefix, unknown names fail as they always have; a namespace
/// isn't thenable, so awaiting one doesn't call it.
#[tokio::test]
async fn unknown_names_outside_a_prefix_still_fail() {
    let only_svc = Sandbox::embedded()
        .javascript()
        .with_callback(tagged_prefix("tools.svc", "svc"))
        .build()
        .unwrap();
    for (code, expected) in [
        ("await nope.tool()", "nope is not defined"),
        (
            "await tools.other.tool()",
            "cannot read property 'tool' of undefined",
        ),
        ("await invoke('nope')", "nope"),
    ] {
        let err = run(&only_svc, code).await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains(expected),
            "{code}: {err}"
        );
    }
    let out = run(
        &only_svc,
        "const ns = await tools.svc; console.log(typeof ns, typeof tools.svc.x.y)",
    )
    .await
    .unwrap();
    assert_eq!(out, "function function\n");
}

/// Printing or serializing a namespace never calls the host: JSON.stringify
/// asks every object for `toJSON`, and `bind` isn't a call.
#[tokio::test]
async fn looking_at_a_namespace_calls_nothing() {
    let calls = Arc::new(AtomicU32::new(0));
    let counted = |prefix: &str| {
        let calls = calls.clone();
        PrefixCallback::new(prefix, "Counted tools", move |name, _args, _pos| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(json!(name)) })
        })
    };
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_callback(counted("mcp.github"))
        .with_callback(counted("mcp.grafana"))
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r"
            console.log(mcp, mcp.github, mcp.github.search);
            console.log(JSON.stringify({ svc: mcp.github }), String(mcp.github.search), `${mcp.github}`);
            const bound = mcp.github.search.bind(null);
            console.log(typeof bound, typeof mcp.github.call, typeof mcp.github.toJSON);
            var result = { svc: mcp.grafana };
            ",
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        out.stdout_text(),
        "{} [Namespace mcp.github] [Namespace mcp.github.search]\n\
         {} [Namespace mcp.github.search] [Namespace mcp.github]\n\
         function function undefined\n"
    );
    assert_eq!(out.result.as_deref(), Some("{}"));
    // And `invoke` still reaches a tool with one of those names.
    let out = sandbox
        .execute("console.log(await invoke('mcp.github.toJSON'))")
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "mcp.github.toJSON\n");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A namespace is an installed callback, not session state.
#[tokio::test]
async fn prefix_namespaces_are_not_session_state() {
    let sandbox = sandbox();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("const r = await mcp.svc.tool({ a: 1 }); var hit = r.hit")
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    assert_eq!(snapshot.skipped(), &[] as &[String]);

    let mut restored = InProcessSession::new(&sandbox).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    let out = restored
        .execute("console.log(hit, (await mcp.svc.again()).hit)")
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "svc:mcp.svc.tool svc:mcp.svc.again\n");
}
