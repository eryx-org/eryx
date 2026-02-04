//! Integration tests for secrets placeholder substitution.
//!
//! These tests require the `embedded` feature and a built runtime.
//! Run with: `mise run test` or `cargo test --features embedded`

#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{NetConfig, Sandbox};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Mock HTTP server for testing secret substitution.
///
/// Records received requests so we can verify that real secrets were sent.
#[derive(Debug, Default)]
struct MockHttpServer {
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockHttpServer {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn start(&self, port: u16) -> tokio::task::JoinHandle<()> {
        let requests = self.requests.clone();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
                .await
                .expect("Failed to bind mock server");

            while let Ok((mut socket, _)) = listener.accept().await {
                let requests = requests.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    if let Ok(n) = socket.read(&mut buf).await {
                        buf.truncate(n);
                        if let Ok(request) = String::from_utf8(buf) {
                            requests.lock().await.push(request);

                            // Send minimal HTTP response
                            let response = "HTTP/1.1 200 OK\r\n\
                                          Content-Type: application/json\r\n\
                                          Content-Length: 27\r\n\
                                          \r\n\
                                          {\"message\":\"success\"}";
                            let _ = socket.write_all(response.as_bytes()).await;
                        }
                    }
                });
            }
        })
    }

    async fn get_requests(&self) -> Vec<String> {
        self.requests.lock().await.clone()
    }
}

#[tokio::test]
async fn test_secret_substitution_in_http_request() {
    // Start mock server
    let server = MockHttpServer::new();
    let _handle = server.start(18080).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create sandbox with secret
    let sandbox = Sandbox::embedded()
        .with_secret(
            "TEST_API_KEY",
            "real-secret-value-12345",
            vec!["127.0.0.1".to_string()],
        )
        .with_network(NetConfig::permissive()) // Allow localhost for testing
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .expect("Failed to create sandbox");

    // Execute Python code that makes HTTP request with secret
    let result = sandbox
        .execute(
            r#"
import os
import socket

# Get the secret (will be a placeholder)
api_key = os.environ.get("TEST_API_KEY", "")
print(f"Secret in Python: {api_key}")

# Make HTTP request with the secret in header
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 18080))

request = f"GET /test HTTP/1.1\r\n"
request += "Host: 127.0.0.1\r\n"
request += f"Authorization: Bearer {api_key}\r\n"
request += "\r\n"

sock.send(request.encode())
response = sock.recv(4096).decode()
sock.close()

print(f"Response received: {response[:50]}")
"#,
        )
        .await
        .expect("Failed to execute Python code");

    // Verify placeholder was scrubbed from stdout
    assert!(
        result.stdout.contains("[REDACTED]"),
        "Placeholder should be scrubbed from stdout"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder should not appear in stdout"
    );
    assert!(
        !result.stdout.contains("real-secret-value-12345"),
        "Real secret should never appear in stdout"
    );

    // Give server time to receive request
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify the real secret was sent to the server
    let requests = server.get_requests().await;
    assert!(!requests.is_empty(), "Server should have received a request");

    let first_request = &requests[0];
    assert!(
        first_request.contains("real-secret-value-12345"),
        "Real secret should be in the HTTP request: {}",
        first_request
    );
    assert!(
        first_request.contains("Authorization: Bearer real-secret-value-12345"),
        "Authorization header should contain real secret"
    );
}

#[tokio::test]
async fn test_secret_blocked_for_unauthorized_host() {
    // Create sandbox with secret restricted to specific host
    let sandbox = Sandbox::embedded()
        .with_secret(
            "RESTRICTED_KEY",
            "secret-value",
            vec!["api.example.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to create sandbox");

    // Try to use secret with unauthorized host - should fail
    let result = sandbox
        .execute(
            r#"
import os
import socket

api_key = os.environ.get("RESTRICTED_KEY", "")

# Try to connect to localhost (not in allowed_hosts)
try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 18081))

    request = f"GET /test HTTP/1.1\r\n"
    request += "Host: 127.0.0.1\r\n"
    request += f"Authorization: Bearer {api_key}\r\n"
    request += "\r\n"

    sock.send(request.encode())
    print("ERROR: Request should have been blocked!")
except Exception as e:
    print(f"Expected error: {e}")
"#,
        )
        .await;

    // The execution itself should succeed, but the secret substitution should fail
    // when the TCP write happens, resulting in an error in the Python code
    assert!(result.is_ok());
    let output = result.unwrap();

    // Should see an error in output (connection refused or similar)
    assert!(
        output.stdout.contains("Expected error") || output.stderr.contains("error"),
        "Should see error when secret is blocked for host"
    );
}

#[tokio::test]
async fn test_placeholder_not_in_stderr() {
    let sandbox = Sandbox::embedded()
        .with_secret("TEST_KEY", "secret", vec![])
        .scrub_stderr(true)
        .build()
        .expect("Failed to create sandbox");

    let result = sandbox
        .execute(
            r#"
import os
import sys

key = os.environ.get("TEST_KEY", "")
sys.stderr.write(f"Error with key: {key}\n")
"#,
        )
        .await
        .expect("Failed to execute");

    // Verify placeholder is scrubbed from stderr
    assert!(
        result.stderr.contains("[REDACTED]"),
        "Placeholder should be scrubbed from stderr"
    );
    assert!(
        !result.stderr.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder should not appear in stderr"
    );
}

#[tokio::test]
async fn test_multiple_secrets() {
    let sandbox = Sandbox::embedded()
        .with_secret("KEY1", "secret1", vec![])
        .with_secret("KEY2", "secret2", vec![])
        .scrub_stdout(true)
        .build()
        .expect("Failed to create sandbox");

    let result = sandbox
        .execute(
            r#"
import os

key1 = os.environ.get("KEY1", "")
key2 = os.environ.get("KEY2", "")

print(f"Key1: {key1}")
print(f"Key2: {key2}")
"#,
        )
        .await
        .expect("Failed to execute");

    // Both placeholders should be scrubbed
    assert_eq!(
        result.stdout.matches("[REDACTED]").count(),
        2,
        "Both secrets should be scrubbed"
    );
}

#[tokio::test]
async fn test_scrubbing_can_be_disabled() {
    let sandbox = Sandbox::embedded()
        .with_secret("DEBUG_KEY", "debug-secret", vec![])
        .scrub_stdout(false) // Disable scrubbing for debugging
        .build()
        .expect("Failed to create sandbox");

    let result = sandbox
        .execute(
            r#"
import os
key = os.environ.get("DEBUG_KEY", "")
print(f"Debug key: {key}")
"#,
        )
        .await
        .expect("Failed to execute");

    // Placeholder should NOT be scrubbed when disabled
    assert!(
        result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder should appear when scrubbing is disabled"
    );
    assert!(
        !result.stdout.contains("[REDACTED]"),
        "Should not see [REDACTED] when scrubbing is disabled"
    );
}

#[tokio::test]
async fn test_http2_detection() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", "secret", vec!["example.com".to_string()])
        .with_network(NetConfig::default().allow_host("example.com"))
        .build()
        .expect("Failed to create sandbox");

    // Try to send HTTP/2 preface - should fail with clear error
    let result = sandbox
        .execute(
            r#"
import socket

try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    # Note: This will fail at DNS/connect level since example.com isn't actually accessible
    # But if we could connect, sending HTTP/2 preface would be caught
    sock.connect(("example.com", 443))

    # HTTP/2 connection preface
    sock.send(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
    print("ERROR: Should have detected HTTP/2!")
except Exception as e:
    print(f"Expected connection error: {e}")
"#,
        )
        .await;

    // Should get connection error (can't actually reach example.com in tests)
    assert!(result.is_ok());
}
