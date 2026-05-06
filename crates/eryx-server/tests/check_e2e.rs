//! End-to-end gRPC tests for the Check and Format RPCs.
//!
//! These tests start a tonic server in-process and exercise the unary Check
//! and Format RPCs. Unlike the Execute tests, these do NOT require the WASM
//! runtime — type checking and formatting are pure Rust via ty/ruff.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::proto::eryx::v1::{
    CallbackDeclaration, CheckRequest, FileKind, FormatRequest, ParameterDeclaration,
    SupportingFile,
};
use eryx_server::service::EryxService;
use tokio::net::TcpListener;
use tonic::transport::{Channel, Server};

/// Start an in-process gRPC server on a random port and return the channel.
///
/// Note: We still create a sandbox pool because `EryxService` requires one for
/// the Execute RPC. The Check RPC doesn't use it.
async fn start_server() -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let pool_config = PoolConfig {
        max_size: 1,
        min_idle: 0,
        ..Default::default()
    };

    let pool = SandboxPool::new(Sandbox::embedded(), pool_config)
        .await
        .expect("failed to create sandbox pool");
    let pool = Arc::new(pool);

    tokio::spawn(async move {
        Server::builder()
            .add_service(EryxServer::new(EryxService::new(pool)))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn check_valid_code_no_diagnostics() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "x: int = 42\nprint(x)\n".to_string(),
            files: vec![],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        type_errors.is_empty(),
        "expected no type errors, got: {type_errors:?}"
    );
}

#[tokio::test]
async fn check_syntax_error() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "def foo(\n".to_string(),
            files: vec![],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    assert!(
        !resp.diagnostics.is_empty(),
        "expected syntax diagnostics for incomplete def"
    );
    // Syntax errors come from the type checker as well.
    let has_error = resp.diagnostics.iter().any(|d| d.severity == "error");
    assert!(has_error, "expected at least one error-level diagnostic");
}

#[tokio::test]
async fn check_type_error() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "x: int = 'hello'\n".to_string(),
            files: vec![],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        !type_errors.is_empty(),
        "expected type error for str assigned to int, got: {:?}",
        resp.diagnostics
    );
}

#[tokio::test]
async fn check_with_supporting_module() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "from helpers import add\nx: int = add(1, 2)\n".to_string(),
            files: vec![SupportingFile {
                name: "helpers.py".to_string(),
                content: "def add(a: int, b: int) -> int:\n    return a + b\n".to_string(),
                kind: FileKind::Module as i32,
            }],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        type_errors.is_empty(),
        "expected no type errors with valid helper import, got: {type_errors:?}"
    );
}

#[tokio::test]
async fn check_data_files_ignored() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    // DATA files should be ignored — importing them should cause an error.
    let resp = client
        .check(CheckRequest {
            code: "from data_helpers import something\n".to_string(),
            files: vec![SupportingFile {
                name: "data_helpers.py".to_string(),
                content: "something = 42\n".to_string(),
                kind: FileKind::Data as i32,
            }],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    let has_error = resp.diagnostics.iter().any(|d| d.severity == "error");
    assert!(
        has_error,
        "expected import error since DATA files are not importable, got: {:?}",
        resp.diagnostics
    );
}

#[tokio::test]
async fn check_callback_correct_usage() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "async def main():\n    result = await query_loki(expr='up{job=\"foo\"}')\n"
                .to_string(),
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query_loki".to_string(),
                description: "Query Loki logs".to_string(),
                parameters: vec![ParameterDeclaration {
                    name: "expr".to_string(),
                    json_type: "string".to_string(),
                    description: "LogQL expression".to_string(),
                    required: true,
                }],
            }],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        type_errors.is_empty(),
        "expected no type errors for correct callback usage, got: {type_errors:?}"
    );
}

#[tokio::test]
async fn check_callback_wrong_arg_type() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "async def main():\n    result = await query_loki(expr=42)\n".to_string(),
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query_loki".to_string(),
                description: "Query Loki logs".to_string(),
                parameters: vec![ParameterDeclaration {
                    name: "expr".to_string(),
                    json_type: "string".to_string(),
                    description: "LogQL expression".to_string(),
                    required: true,
                }],
            }],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        !type_errors.is_empty(),
        "expected type error for int passed as str, got: {:?}",
        resp.diagnostics
    );
}

#[tokio::test]
async fn check_callback_with_optional_params() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "async def main():\n    result = await query(expr='test')\n".to_string(),
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query".to_string(),
                description: "Query with optional limit".to_string(),
                parameters: vec![
                    ParameterDeclaration {
                        name: "expr".to_string(),
                        json_type: "string".to_string(),
                        description: "Query expression".to_string(),
                        required: true,
                    },
                    ParameterDeclaration {
                        name: "limit".to_string(),
                        json_type: "integer".to_string(),
                        description: "Max results".to_string(),
                        required: false,
                    },
                ],
            }],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        type_errors.is_empty(),
        "omitting optional param should not cause type error, got: {type_errors:?}"
    );
}

/// Eryx scripts support top-level await (TLA) — no need for `async def main()`.
#[tokio::test]
async fn check_callback_top_level_await() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .check(CheckRequest {
            code: "result = await query_prom(expr='up')\nprint(result)\n".to_string(),
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query_prom".to_string(),
                description: "Query Prometheus".to_string(),
                parameters: vec![ParameterDeclaration {
                    name: "expr".to_string(),
                    json_type: "string".to_string(),
                    description: "PromQL expression".to_string(),
                    required: true,
                }],
            }],
        })
        .await
        .unwrap()
        .into_inner();

    let type_errors: Vec<_> = resp
        .diagnostics
        .iter()
        .filter(|d| d.source == "type")
        .collect();
    assert!(
        type_errors.is_empty(),
        "top-level await should not cause type errors, got: {type_errors:?}"
    );
}

#[tokio::test]
async fn check_diagnostic_offsets_are_valid() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let code = "x: int = 'hello'\n";
    let resp = client
        .check(CheckRequest {
            code: code.to_string(),
            files: vec![],
            callbacks: vec![],
        })
        .await
        .unwrap()
        .into_inner();

    assert!(!resp.diagnostics.is_empty(), "expected diagnostics");
    for diag in &resp.diagnostics {
        assert!(
            (diag.end_offset as usize) <= code.len(),
            "diagnostic end_offset {} exceeds source length {}: {:?}",
            diag.end_offset,
            code.len(),
            diag
        );
    }
}

// ─── Format RPC tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn format_fixes_whitespace() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .format(FormatRequest {
            code: "x=1\ny =  2\n".to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(resp.error.is_empty(), "unexpected error: {}", resp.error);
    assert_eq!(resp.formatted_code, "x = 1\ny = 2\n");
}

#[tokio::test]
async fn format_already_clean() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let source = "x = 1\nprint(x)\n";
    let resp = client
        .format(FormatRequest {
            code: source.to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(resp.error.is_empty());
    assert_eq!(resp.formatted_code, source);
}

#[tokio::test]
async fn format_syntax_error_returns_error() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let resp = client
        .format(FormatRequest {
            code: "def foo(\n".to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(
        !resp.error.is_empty(),
        "expected error for syntax-invalid code"
    );
    assert!(
        resp.formatted_code.is_empty(),
        "formatted_code should be empty on error"
    );
}

#[tokio::test]
async fn format_multiline_function() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let messy = "def foo( a,b,  c ):\n  return a+b+c\n";
    let resp = client
        .format(FormatRequest {
            code: messy.to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(resp.error.is_empty(), "unexpected error: {}", resp.error);
    // Ruff formatter normalizes spacing.
    assert!(
        resp.formatted_code.contains("def foo(a, b, c)"),
        "expected normalized function signature, got: {:?}",
        resp.formatted_code
    );
}
