//! `with_callback_namespace` (#964): callbacks under one name in every guest.
//!
//! ```sh
//! cargo nextest run -p eryx --features embedded,javascript,shell --test callback_namespace
//! ```
#![cfg(all(feature = "embedded", feature = "javascript", feature = "shell"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{
    Callback, CallbackError, DynamicCallback, PoolConfig, PrefixCallback, Sandbox, SandboxPool,
};
use serde_json::json;

/// `add`, with parameters `a` and `b`.
fn add() -> DynamicCallback {
    DynamicCallback::builder("add", "Adds a and b", |args| {
        Box::pin(async move {
            Ok::<_, CallbackError>(json!(
                args["a"].as_i64().unwrap() + args["b"].as_i64().unwrap()
            ))
        })
    })
    .param("a", "integer", "", true)
    .param("b", "integer", "", true)
    .build()
}

/// A prefix callback `mcp.*` returning the name it was handed.
fn mcp() -> PrefixCallback {
    PrefixCallback::new("mcp", "MCP tools", |name, _, _| {
        Box::pin(async move { Ok(json!(name)) })
    })
}

/// A callback named like a guest's own global.
fn named(name: &str) -> DynamicCallback {
    DynamicCallback::builder(name, "", |_| Box::pin(async { Ok(json!(1)) })).build()
}

#[tokio::test]
async fn python_calls_callbacks_under_the_namespace() {
    let sandbox = Sandbox::embedded()
        .with_callback_namespace("tools")
        .with_callback(add())
        .with_callback(mcp())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "print(await tools.add(a=2, b=3), await tools.add(4, 5))\n\
             print(await tools.mcp.github.search(q='x'))\n\
             print('add' in globals(), 'mcp' in globals())\n\
             print(sorted(c['name'] for c in list_callbacks()))",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_text(),
        "5 9\nmcp.github.search\nFalse False\n['tools.add', 'tools.mcp.*']\n"
    );
}

#[tokio::test]
async fn javascript_calls_callbacks_under_the_namespace() {
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_callback_namespace("tools")
        .with_callback(add())
        .with_callback(mcp())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "console.log(await tools.add({ a: 2, b: 3 }), await tools.add(4, 5));\n\
             console.log(await tools.mcp.github.search({ q: 'x' }));\n\
             console.log(typeof add, typeof mcp);",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_text(),
        "5 9\nmcp.github.search\nundefined undefined\n"
    );
}

#[tokio::test]
async fn shell_runs_callbacks_as_dotted_commands() {
    let sandbox = Sandbox::embedded()
        .shell()
        .with_callback_namespace("tools")
        .with_callback(add())
        .with_callback(mcp())
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            "tools.add --a 2 --b 3\n\
             invoke tools.mcp.github.search '{}'\n\
             add --a 1 --b 1 2>/dev/null || echo \"add: $?\"",
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "5\nmcp.github.search\nadd: 127\n"
    );
}

/// Only the namespace has to stay clear of the guest's names: `fetch` is
/// fine under `tools`, but a namespace named like a global isn't.
#[tokio::test]
async fn the_namespace_is_checked_rather_than_each_callback() {
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_callback_namespace("tools")
        .with_callback(named("fetch"))
        .build()
        .unwrap();
    let out = sandbox
        .execute("console.log(await tools.fetch(), typeof fetch)")
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "1 function\n");

    let err = Sandbox::embedded()
        .javascript()
        .with_callback_namespace("crypto")
        .with_callback(add())
        .build()
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("callback 'crypto.add' collides with the JavaScript global 'crypto'"),
        "{err}"
    );
    let err = Sandbox::embedded()
        .with_callback_namespace("to.ols")
        .build()
        .unwrap_err();
    assert!(
        err.to_string().contains("invalid callback namespace"),
        "{err}"
    );
}

/// A pool built with a namespace puts each lease's callbacks under it too.
#[tokio::test]
async fn pooled_callbacks_go_under_the_namespace() {
    let pool = SandboxPool::new(
        Sandbox::embedded().with_callback_namespace("tools"),
        PoolConfig::default().with_max_size(1),
    )
    .await
    .unwrap();
    let sandbox = pool
        .acquire()
        .await
        .unwrap()
        .with_callbacks(vec![Box::new(add()) as Box<dyn Callback>])
        .unwrap();
    let out = sandbox
        .execute("print(await tools.add(a=1, b=2))")
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "3\n");
}
