//! Capturing a structured result from a script.
//!
//! Besides stdout/stderr, a script can hand a structured value back to the host
//! by assigning a variable named `result` (configurable). After execution it is
//! JSON-serialized and returned as `ExecuteResult::result` — a clean data channel
//! you don't have to parse out of printed output.
//!
//! The variable is *consumed* after each execution: in a persistent session (via
//! `SessionExecutor`) a later run that doesn't set it reports `None` rather than
//! re-reporting a stale value. Each `Sandbox::execute` below runs in a fresh
//! instance, so that detail isn't visible here — see the session tests for it.
//!
//! Run with: `cargo run --example result_capture --features=embedded`

use eryx::Sandbox;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Example 1: the happy path — `result` is JSON-serialized for the host.
    println!("=== Example 1: capturing a structured result ===");
    let sandbox = Sandbox::embedded().build()?;
    let out = sandbox
        .execute(
            r#"
items = [n * n for n in range(5)]
result = {"squares": items, "total": sum(items)}
"#,
        )
        .await?;
    // `result` is the JSON string; parse it however you like (here with serde_json).
    let json = out
        .result
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("script did not set `result`"))?;
    let value: serde_json::Value = serde_json::from_str(json)?;
    println!("result.total = {}", value["total"]);
    println!("raw JSON      = {json}");
    println!();

    // Example 2: a value that isn't JSON-serializable. Execution still succeeds;
    // `result` is None and `result_error` explains why. This keeps result capture
    // a soft side channel — a bad result never fails the run.
    println!("=== Example 2: non-serializable result ===");
    let out = sandbox.execute("result = object()").await?;
    println!("result       = {:?}", out.result);
    println!("result_error = {:?}", out.result_error);
    println!();

    // Example 3: capture a differently-named variable. A plain `result` is ignored
    // when a custom name is configured.
    println!("=== Example 3: custom result variable name ===");
    let sandbox = Sandbox::embedded().with_result_variable("answer").build()?;
    let out = sandbox.execute("answer = 42\nresult = 'ignored'").await?;
    println!("captured `answer` = {:?}", out.result.as_deref());

    Ok(())
}
