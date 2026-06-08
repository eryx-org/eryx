//! Integration tests for the structured `result` capture feature.
//!
//! After a script runs, the variable named `result` (configurable via
//! `SandboxBuilder::with_result_variable`) is JSON-serialized and returned in
//! `ExecuteResult::result`. Non-serializable values surface in `result_error`
//! without failing execution.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "embedded")]

use eryx::Sandbox;

/// A `result` dict round-trips as JSON.
#[tokio::test]
async fn captures_result_as_json() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox
        .execute("result = {\"a\": 1, \"b\": [2, 3]}")
        .await
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("{\"a\": 1, \"b\": [2, 3]}"));
    assert!(out.result_error.is_none());
}

/// No `result` variable -> both fields are `None`.
#[tokio::test]
async fn no_result_variable_is_none() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("print('hi')").await.unwrap();
    assert!(out.result.is_none());
    assert!(out.result_error.is_none());
}

/// A `result = None` is captured as the JSON literal `null` (distinct from absent).
#[tokio::test]
async fn result_none_is_json_null() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("result = None").await.unwrap();
    assert_eq!(out.result.as_deref(), Some("null"));
    assert!(out.result_error.is_none());
}

/// A non-JSON-serializable `result` reports `result_error` but still succeeds.
#[tokio::test]
async fn non_serializable_result_reports_error() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("result = object()").await.unwrap();
    assert!(out.result.is_none());
    let err = out.result_error.expect("expected a result_error");
    assert!(
        err.contains("not JSON-serializable"),
        "unexpected result_error: {err}"
    );
}

/// `result` populated through a top-level `await` (the async suspend/resume path).
#[tokio::test]
async fn captures_result_from_async_code() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox
        .execute("import asyncio\nawait asyncio.sleep(0)\nresult = 42")
        .await
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("42"));
    assert!(out.result_error.is_none());
}

/// A custom result-variable name is captured; a plain `result` is ignored.
#[tokio::test]
async fn custom_result_variable_name() {
    let sandbox = Sandbox::embedded()
        .with_result_variable("out")
        .build()
        .unwrap();
    let out = sandbox.execute("out = 42\nresult = 1").await.unwrap();
    assert_eq!(out.result.as_deref(), Some("42"));
    assert!(out.result_error.is_none());
}
