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

/// A non-finite float (`inf`/`nan`) is rejected rather than emitting the invalid
/// JSON tokens `Infinity`/`NaN` (json.dumps with allow_nan=False).
#[tokio::test]
async fn non_finite_float_reports_error() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("result = float('inf')").await.unwrap();
    assert!(out.result.is_none());
    assert!(
        out.result_error.is_some(),
        "expected a result_error for non-finite float"
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

/// An empty-string result is captured as the JSON literal `""` (two characters),
/// which must stay distinct from "no result set". This pins the core sentinel
/// invariant: a JSON value is never the empty string, so empty == absent is safe.
#[tokio::test]
async fn empty_string_result_is_distinct_from_absent() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("result = ''").await.unwrap();
    assert_eq!(out.result.as_deref(), Some("\"\""));
    assert!(out.result_error.is_none());
}

/// Falsy values (`0`, `False`, `[]`, `{}`) are captured, not mistaken for absent.
/// The guest tests membership (`name in globals`), not truthiness, so these must
/// round-trip to their JSON literals.
#[tokio::test]
async fn falsy_results_are_captured() {
    let sandbox = Sandbox::embedded().build().unwrap();
    for (code, expected) in [
        ("result = 0", "0"),
        ("result = False", "false"),
        ("result = []", "[]"),
        ("result = {}", "{}"),
        ("result = 0.0", "0.0"),
    ] {
        let out = sandbox.execute(code).await.unwrap();
        assert_eq!(out.result.as_deref(), Some(expected), "for code: {code}");
        assert!(out.result_error.is_none(), "for code: {code}");
    }
}

/// Common non-JSON-serializable types each report a `result_error` (naming the
/// type) without failing execution: bytes, set, and a dict with a tuple key.
#[tokio::test]
async fn common_non_serializable_types_report_error() {
    let sandbox = Sandbox::embedded().build().unwrap();
    for code in [
        "result = b'bytes'",
        "result = {1, 2, 3}",
        "result = {(1, 2): 'x'}",
    ] {
        let out = sandbox.execute(code).await.unwrap();
        assert!(out.result.is_none(), "result should be None for: {code}");
        let err = out
            .result_error
            .unwrap_or_else(|| panic!("expected result_error for: {code}"));
        assert!(
            err.contains("not JSON-serializable"),
            "unexpected result_error for {code}: {err}"
        );
    }
}

/// A custom result-variable name containing Python string-literal metacharacters
/// (quotes, a statement separator, triple-quotes, a backslash) must be escaped
/// when assigned in the guest — never executed. If injection were possible this
/// would either run code or break the interpreter; instead the name simply never
/// matches a real variable, so the result is absent.
#[tokio::test]
async fn result_variable_name_is_not_injectable() {
    let malicious = "x\"\"\"; import os; raise SystemExit('INJECTED'); y = \"\"\"\\";
    let sandbox = Sandbox::embedded()
        .with_result_variable(malicious)
        .build()
        .unwrap();
    // Execution succeeds normally; the weird name matches nothing, so no result.
    let out = sandbox.execute("result = 1\nprint('alive')").await.unwrap();
    assert_eq!(out.stdout, "alive");
    assert!(out.result.is_none());
    assert!(out.result_error.is_none());
}

/// Unicode round-trips through the (ensure_ascii=True) JSON encoding: the raw
/// JSON escapes non-ASCII to \uXXXX, and re-parsing recovers the original text.
#[tokio::test]
async fn unicode_result_round_trips_escaped() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox
        .execute("result = {'msg': 'café \\U0001f600'}")
        .await
        .unwrap();
    let json = out.result.expect("expected a result");
    // ensure_ascii=True => the emoji and accented char are \u-escaped in the wire form.
    assert!(
        json.contains("\\u"),
        "expected escaped non-ASCII, got: {json}"
    );
    assert!(out.result_error.is_none());
    // And it parses back to the original value.
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["msg"], "café 😀");
}

/// A large integer is preserved exactly in the JSON string at the Rust layer
/// (no float coercion — that risk only exists in JS's JSON.parse).
#[tokio::test]
async fn large_integer_is_exact() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let out = sandbox.execute("result = 2 ** 63").await.unwrap();
    assert_eq!(out.result.as_deref(), Some("9223372036854775808"));
    assert!(out.result_error.is_none());
}
