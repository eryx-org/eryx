//! Prefix callbacks: one callback (`mcp.grafana.*`) that handles every dotted
//! name under a prefix, receiving the full name the guest called.
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eryx::{
    Callback, CallbackError, DynamicCallback, PoolConfig, PrefixCallback, ResourceLimits, Sandbox,
    SandboxPool, TraceEventKind,
};
use serde_json::json;

/// A prefix callback that returns `"{tag}:{name}"` plus the kwargs it got.
fn tagged_prefix(prefix: &str, tag: &'static str) -> PrefixCallback {
    PrefixCallback::new(prefix, format!("{tag} tools"), move |name, args| {
        Box::pin(async move { Ok(json!({ "hit": format!("{tag}:{name}"), "args": args })) })
    })
}

/// An ordinary callback returning `"exact:{name}"` plus its kwargs, with one
/// declared parameter `q` so positional arguments bind.
fn exact(name: &str) -> DynamicCallback {
    let hit = format!("exact:{name}");
    DynamicCallback::builder(name, "An exact callback", move |args| {
        let hit = hit.clone();
        Box::pin(async move { Ok(json!({ "hit": hit, "args": args })) })
    })
    .param("q", "string", "A query", false)
    .build()
}

async fn run(sandbox: &Sandbox, code: &str) -> String {
    let output = sandbox.execute(code).await.expect("execution succeeds");
    output.stdout_text().trim().to_string()
}

#[tokio::test]
async fn prefix_callback_receives_full_name_and_kwargs() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("mcp.grafana", "grafana"))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        "r = await mcp.grafana.user_info(id=7, verbose=True)\nprint(r['hit'], r['args'])",
    )
    .await;
    assert_eq!(
        out,
        "grafana:mcp.grafana.user_info {'id': 7, 'verbose': True}"
    );
}

#[tokio::test]
async fn exact_beats_prefix_and_longest_prefix_wins() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("mcp", "outer"))
        .with_callback(tagged_prefix("mcp.grafana", "inner"))
        .with_callback(exact("mcp.grafana.search"))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
print((await mcp.grafana.search(q="x"))['hit'])
print((await mcp.grafana.search("pos"))['args'])
print((await mcp.grafana.other())['hit'])
print((await mcp.github.issues())['hit'])
print((await invoke("mcp.grafana.direct"))['hit'])
"#,
    )
    .await;
    assert_eq!(
        out,
        "exact:mcp.grafana.search\n\
         {'q': 'pos'}\n\
         inner:mcp.grafana.other\n\
         outer:mcp.github.issues\n\
         inner:mcp.grafana.direct"
    );
}

#[tokio::test]
async fn attribute_item_and_nested_access() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("mcp.grafana", "g"))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
print((await mcp.grafana["search-docs"](query="x"))['hit'])
print((await mcp["grafana"].user_info())['hit'])
print((await mcp.grafana.deep.er["tool-name"]())['hit'])
tool = mcp.grafana.saved
print((await tool())['hit'])
"#,
    )
    .await;
    assert_eq!(
        out,
        "g:mcp.grafana.search-docs\n\
         g:mcp.grafana.user_info\n\
         g:mcp.grafana.deep.er.tool-name\n\
         g:mcp.grafana.saved"
    );
}

#[tokio::test]
async fn positional_args_under_a_prefix_are_rejected() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("mcp.grafana", "g"))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
try:
    await mcp.grafana.user_info(1)
except TypeError as e:
    print(e)
"#,
    )
    .await;
    assert!(
        out.starts_with("mcp.grafana.user_info() takes keyword arguments only"),
        "got: {out}"
    );
}

#[tokio::test]
async fn names_outside_every_prefix_still_fail() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("mcp.grafana", "g"))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
for name in ["mcp.github.issues", "mcp.grafana", "mcp.grafana.*", "mcp.grafana.", "other"]:
    try:
        await invoke(name)
        print("UNEXPECTED", name)
    except Exception as e:
        print(e)
"#,
    )
    .await;
    assert_eq!(
        out,
        "Callback 'mcp.github.issues' not found\n\
         Callback 'mcp.grafana' not found\n\
         Callback 'mcp.grafana.*' not found\n\
         Callback 'mcp.grafana.' not found\n\
         Callback 'other' not found"
    );
}

#[tokio::test]
async fn handler_errors_and_list_callbacks() {
    let strict = PrefixCallback::new("tools", "Known tools only", |name, _args| {
        Box::pin(async move {
            match name.as_str() {
                "tools.ping" => Ok(json!("pong")),
                _ => Err(CallbackError::NotFound(name)),
            }
        })
    });
    let sandbox = Sandbox::embedded().with_callback(strict).build().unwrap();

    let out = run(
        &sandbox,
        r#"
print(await tools.ping())
try:
    await tools.nope()
except Exception as e:
    print(e)
print([(c['name'], c['description']) for c in list_callbacks()])
"#,
    )
    .await;
    assert_eq!(
        out,
        "pong\n\
         callback not found: tools.nope\n\
         [('tools.*', 'Known tools only')]"
    );
}

#[tokio::test]
async fn trace_and_journal_use_the_full_name() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let prefix = PrefixCallback::new("mcp.grafana", "g", move |name, _args| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(json!(format!("{name}#{n}"))) })
    });
    let sandbox = Sandbox::embedded()
        .with_callback(prefix)
        .with_trace_collection(true)
        .build()
        .unwrap();

    let code = "print(await mcp.grafana.a(), await mcp.grafana.b())";
    let first = sandbox.execute_with_journal(code, None).await;
    let output = first.result.expect("first run succeeds");
    assert_eq!(
        output.stdout_text().trim(),
        "mcp.grafana.a#1 mcp.grafana.b#2"
    );
    let names: Vec<_> = first
        .journal
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["mcp.grafana.a", "mcp.grafana.b"]);
    let starts: Vec<_> = output
        .trace
        .iter()
        .filter_map(|e| match &e.event {
            TraceEventKind::CallbackStart { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, ["mcp.grafana.a", "mcp.grafana.b"]);

    // Replaying serves both names from the journal without calling the handler.
    let second = sandbox
        .execute_with_journal(code, Some(first.journal))
        .await;
    let output = second.result.expect("replay succeeds");
    assert_eq!(
        output.stdout_text().trim(),
        "mcp.grafana.a#1 mcp.grafana.b#2"
    );
    assert_eq!(second.replayed_callbacks, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn prefix_counts_toward_max_callback_invocations() {
    let sandbox = Sandbox::embedded()
        .with_callback(tagged_prefix("t", "t"))
        .with_resource_limits(ResourceLimits::default().with_max_callback_invocations(2))
        .build()
        .unwrap();

    let out = run(
        &sandbox,
        r#"
for name in ["a", "b", "c"]:
    try:
        print((await t[name]())['hit'])
    except Exception as e:
        print(e)
"#,
    )
    .await;
    assert_eq!(out, "t:t.a\nt:t.b\nCallback limit exceeded (2 invocations)");
}

/// `n` ordinary callbacks under `mcp.grafana`, each with a description and a
/// small schema, as an MCP bridge would register them.
fn many_tools(n: usize) -> Vec<Box<dyn Callback>> {
    (0..n)
        .map(|i| {
            Box::new(
                DynamicCallback::builder(
                    format!("mcp.grafana.tool_{i}"),
                    format!(
                        "Tool number {i}: does something useful with a dashboard or datasource"
                    ),
                    |args| Box::pin(async move { Ok(args) }),
                )
                .param("query", "string", "What to look for", true)
                .param("limit", "integer", "How many results to return", false)
                .param("datasource_uid", "string", "The datasource to use", false)
                .build(),
            ) as Box<dyn Callback>
        })
        .collect()
}

/// Fuel one pooled execution of `code` uses with `callbacks` installed.
async fn pooled_fuel(pool: &SandboxPool, callbacks: Vec<Box<dyn Callback>>, code: &str) -> u64 {
    let sandbox = pool
        .acquire()
        .await
        .unwrap()
        .with_callbacks(callbacks)
        .unwrap();
    let output = sandbox.execute(code).await.expect("execution succeeds");
    output.stats.fuel_consumed.expect("fuel is tracked")
}

#[tokio::test]
async fn pooled_prefix_callback_is_cheap() {
    let pool = SandboxPool::new(
        Sandbox::embedded(),
        PoolConfig::default().with_max_size(1).with_min_idle(1),
    )
    .await
    .unwrap();
    let prefix = || vec![Box::new(tagged_prefix("mcp.grafana", "g")) as Box<dyn Callback>];
    let single = || vec![Box::new(exact("mcp.grafana.search")) as Box<dyn Callback>];

    let call = "await mcp.grafana.tool_3(query='x')";
    let none = pooled_fuel(&pool, Vec::new(), "pass").await;
    let one = pooled_fuel(&pool, single(), "pass").await;
    let many = pooled_fuel(&pool, many_tools(109), "pass").await;
    let many_call = pooled_fuel(&pool, many_tools(109), call).await;
    let pre = pooled_fuel(&pool, prefix(), "pass").await;
    let pre_call = pooled_fuel(&pool, prefix(), call).await;
    // The prefix callback works through the pool.
    let sandbox = pool
        .acquire()
        .await
        .unwrap()
        .with_callbacks(prefix())
        .unwrap();
    let out = sandbox
        .execute("print((await mcp.grafana.tool_3(query='x'))['hit'])")
        .await
        .unwrap();
    assert_eq!(out.stdout_text().trim(), "g:mcp.grafana.tool_3");
    drop(sandbox);

    eprintln!(
        "fuel (pooled): pass={none} one-callback={one} 109-callbacks={many} \
         109-callbacks+call={many_call} prefix={pre} prefix+call={pre_call}"
    );
    // One prefix callback costs about what one ordinary callback does, and far
    // less than registering every tool separately.
    assert!(pre < one * 2, "prefix {pre} vs one callback {one}");
    assert!(pre * 5 < many, "prefix {pre} vs 109 callbacks {many}");
    assert!(
        pre_call < many_call,
        "prefix+call {pre_call} vs {many_call}"
    );
}
