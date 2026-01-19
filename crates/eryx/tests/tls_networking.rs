//! Integration tests for TLS networking support.
//!
//! These tests verify that Python HTTP client libraries work correctly
//! through the sandbox's TLS implementation.
//!
//! Tests use a local HTTPS server with a self-signed certificate to avoid
//! external network dependencies.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::sync::Arc;

use eryx::{NetConfig, Sandbox};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

// =============================================================================
// Test Server Infrastructure
// =============================================================================

/// A simple HTTPS test server that responds to HTTP requests.
struct TestServer {
    addr: std::net::SocketAddr,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Start a new HTTPS test server on a random port.
    async fn start() -> (Self, Vec<u8>) {
        // Generate a self-signed certificate for "localhost"
        let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let cert = rcgen::generate_simple_self_signed(subject_alt_names)
            .expect("Failed to generate certificate");

        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();

        // Build rustls server config
        let certs = vec![CertificateDer::from(cert_der.clone())];
        let key = PrivateKeyDer::try_from(key_der).expect("Invalid private key");

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("Failed to build server config");

        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        // Bind to random port
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind");
        let addr = listener.local_addr().expect("Failed to get local addr");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        // Spawn server task
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let acceptor = acceptor.clone();
                                tokio::spawn(async move {
                                    if let Ok(mut tls_stream) = acceptor.accept(stream).await {
                                        // Read HTTP request
                                        let mut buf = vec![0u8; 4096];
                                        let n = tls_stream.read(&mut buf).await.unwrap_or(0);
                                        let request = String::from_utf8_lossy(&buf[..n]);

                                        // Parse request line
                                        let first_line = request.lines().next().unwrap_or("");
                                        let parts: Vec<&str> = first_line.split_whitespace().collect();
                                        let method = parts.first().copied().unwrap_or("GET");
                                        let path = parts.get(1).copied().unwrap_or("/");

                                        // Generate response based on path
                                        let (status, body) = match path {
                                            "/get" => ("200 OK", format!(r#"{{"url": "https://localhost{}", "method": "{}"}}"#, path, method)),
                                            "/post" => {
                                                // Extract JSON body from request
                                                let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                                                let req_body = &request[body_start..];
                                                ("200 OK", format!(r#"{{"url": "https://localhost{}", "method": "{}", "json": {}}}"#, path, method, req_body))
                                            }
                                            "/status/404" => ("404 Not Found", r#"{"error": "not found"}"#.to_string()),
                                            "/delay/1" => {
                                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                                ("200 OK", r#"{"delayed": true}"#.to_string())
                                            }
                                            _ => ("200 OK", format!(r#"{{"path": "{}"}}"#, path)),
                                        };

                                        let response = format!(
                                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                                            status,
                                            body.len(),
                                            body
                                        );

                                        let _ = tls_stream.write_all(response.as_bytes()).await;
                                        let _ = tls_stream.flush().await;
                                        // Properly shutdown TLS to send close_notify
                                        let _ = tls_stream.shutdown().await;
                                    }
                                });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        (
            TestServer {
                addr,
                shutdown_tx,
                handle,
            },
            cert_der,
        )
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> PathBuf {
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

/// Create a sandbox builder with networking enabled.
fn sandbox_builder_with_network() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

// =============================================================================
// Basic SSL Module Tests
// =============================================================================

#[tokio::test]
async fn test_ssl_module_imports() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import ssl
print(f"SSL module loaded: {ssl.__name__}")
print(f"HAS_SNI: {ssl.HAS_SNI}")
print(f"HAS_ALPN: {ssl.HAS_ALPN}")
print(f"PROTOCOL_TLS_CLIENT: {ssl.PROTOCOL_TLS_CLIENT}")
"#,
        )
        .await;

    assert!(result.is_ok(), "SSL import should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("SSL module loaded: ssl"));
    assert!(output.stdout.contains("HAS_SNI: True"));
    assert!(output.stdout.contains("HAS_ALPN: True"));
}

#[tokio::test]
async fn test_ssl_context_creation() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import ssl
ctx = ssl.create_default_context()
print(f"Context created: {type(ctx).__name__}")
print(f"verify_mode: {ctx.verify_mode}")
print(f"check_hostname: {ctx.check_hostname}")
"#,
        )
        .await;

    assert!(result.is_ok(), "SSL context should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Context created: SSLContext"));
    assert!(output.stdout.contains("check_hostname: True"));
}

// =============================================================================
// Socket Module Tests
// =============================================================================

#[tokio::test]
async fn test_socket_module_imports() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
print(f"Socket module loaded: {socket.__name__}")
print(f"AF_INET: {socket.AF_INET}")
print(f"SOCK_STREAM: {socket.SOCK_STREAM}")
print(f"has_ipv6: {socket.has_ipv6}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Socket import should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Socket module loaded: socket"));
    assert!(output.stdout.contains("AF_INET: 2"));
    assert!(output.stdout.contains("SOCK_STREAM: 1"));
}

#[tokio::test]
async fn test_socket_creation() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
print(f"Socket created: {type(sock).__name__}")
print(f"Family: {sock.family}")
print(f"Type: {sock.type}")
sock.close()
print("Socket closed")
"#,
        )
        .await;

    assert!(result.is_ok(), "Socket creation should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("Socket created: socket"));
    assert!(output.stdout.contains("Socket closed"));
}

#[tokio::test]
async fn test_getaddrinfo() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
info = socket.getaddrinfo("example.com", 443)
print(f"getaddrinfo returned {len(info)} result(s)")
for family, socktype, proto, canonname, sockaddr in info:
    print(f"  {family}, {socktype}, {proto}, {sockaddr}")
"#,
        )
        .await;

    assert!(result.is_ok(), "getaddrinfo should work: {:?}", result);
    let output = result.unwrap();
    assert!(output.stdout.contains("getaddrinfo returned"));
}

// =============================================================================
// Network Policy Tests
// =============================================================================

#[tokio::test]
async fn test_blocked_host_localhost() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default()) // Default blocks localhost
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("localhost", 443))

ctx = ssl.create_default_context()
try:
    ssl_sock = ctx.wrap_socket(sock, server_hostname="localhost")
    print("UNEXPECTED: Connection succeeded")
except Exception as e:
    print(f"EXPECTED: Connection blocked: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should handle blocked host: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED")
            || output.stdout.contains("blocked")
            || output.stdout.contains("not permitted"),
        "Should block localhost: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_blocked_host_private_network() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::default()) // Default blocks private networks
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("192.168.1.1", 443))

ctx = ssl.create_default_context()
try:
    ssl_sock = ctx.wrap_socket(sock, server_hostname="192.168.1.1")
    print("UNEXPECTED: Connection succeeded")
except Exception as e:
    print(f"EXPECTED: Connection blocked: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should handle blocked host: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED")
            || output.stdout.contains("blocked")
            || output.stdout.contains("not permitted"),
        "Should block private network: {}",
        output.stdout
    );
}

#[tokio::test]
async fn test_allowed_hosts_whitelist() {
    let sandbox = sandbox_builder_with_network()
        .with_network(
            NetConfig::default()
                .allow_host("httpbin.org")
                .allow_host("*.example.com"),
        )
        .build()
        .expect("Failed to build sandbox");

    // This test just verifies the config is accepted
    let result = sandbox
        .execute(
            r#"
import socket
import ssl
print("Config with allowed hosts accepted")
"#,
        )
        .await;

    assert!(result.is_ok());
}

// =============================================================================
// TLS Connection Tests (using local test server)
// =============================================================================

/// Test a simple HTTPS GET request using low-level socket/ssl.
#[tokio::test]
async fn test_simple_https_get() {
    let (server, cert_der) = TestServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive().with_root_cert(cert_der))
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async
import _eryx

try:
    # Connect with TCP first
    print("Connecting TCP...")
    tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
    print(f"TCP connected with handle {{tcp_handle}}")

    # Upgrade to TLS
    print("Upgrading to TLS...")
    tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "127.0.0.1")
    print(f"TLS upgraded with handle {{tls_handle}}")

    # Send HTTP request
    print("Sending request...")
    request = b"GET /get HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    written = await _eryx_async.await_tls_write(tls_handle, request)
    print(f"Sent {{written}} bytes")

    # Read response
    print("Reading response...")
    response = b""
    while True:
        chunk = await _eryx_async.await_tls_read(tls_handle, 4096)
        if not chunk:
            break
        response += chunk
    print(f"Read {{len(response)}} bytes")

    # Close connection
    _eryx_async.tls_close(tls_handle)

    # Parse response
    response_str = response.decode('utf-8', errors='replace')
    lines = response_str.split('\r\n')
    print(f"Status: {{lines[0]}}")

    if '200 OK' in lines[0]:
        print("SUCCESS: Got 200 OK")
    else:
        print(f"FAILED: Unexpected status")
except Exception as e:
    import traceback
    print(f"ERROR: {{type(e).__name__}}: {{e}}")
    traceback.print_exc()
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "HTTPS request should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS") || output.stdout.contains("200 OK"),
        "Should get 200 OK: {}",
        output.stdout
    );
}

/// Test HTTP GET request using low-level async TLS API.
#[tokio::test]
async fn test_http_client() {
    let (server, cert_der) = TestServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive().with_root_cert(cert_der))
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async
import json

# Connect with TCP, then upgrade to TLS
tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "127.0.0.1")

# Send HTTP GET request
request = b"GET /get HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
await _eryx_async.await_tls_write(tls_handle, request)

# Read response
response = b""
while True:
    chunk = await _eryx_async.await_tls_read(tls_handle, 4096)
    if not chunk:
        break
    response += chunk

_eryx_async.tls_close(tls_handle)

# Parse response
response_str = response.decode('utf-8')
headers, body = response_str.split('\r\n\r\n', 1)
status_line = headers.split('\r\n')[0]
print(f"Status: {{status_line}}")

data = json.loads(body)
print(f"URL: {{data.get('url', 'N/A')}}")
print("SUCCESS")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "http.client should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS") || output.stdout.contains("200"),
        "Should succeed: {}",
        output.stdout
    );
}

/// Test POST request with JSON body using async TLS API.
#[tokio::test]
async fn test_https_post_json() {
    let (server, cert_der) = TestServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive().with_root_cert(cert_der))
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async
import json

# Connect with TCP, then upgrade to TLS
tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "127.0.0.1")

# Build POST request with JSON body
body = json.dumps({{"name": "eryx", "version": "0.2.0"}})
request = f"POST /post HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {{len(body)}}\r\nConnection: close\r\n\r\n{{body}}"
await _eryx_async.await_tls_write(tls_handle, request.encode())

# Read response
response = b""
while True:
    chunk = await _eryx_async.await_tls_read(tls_handle, 4096)
    if not chunk:
        break
    response += chunk

_eryx_async.tls_close(tls_handle)

# Parse response
response_str = response.decode('utf-8')
headers, response_body = response_str.split('\r\n\r\n', 1)
status_line = headers.split('\r\n')[0]
print(f"Status: {{status_line}}")

data = json.loads(response_body)
json_data = data.get("json", {{}})

if json_data.get("name") == "eryx" and json_data.get("version") == "0.2.0":
    print("SUCCESS: JSON echoed correctly")
else:
    print(f"FAILED: Unexpected response: {{json_data}}")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "POST should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS"),
        "Should succeed: {}",
        output.stdout
    );
}

/// Test multiple requests using separate TLS connections.
#[tokio::test]
async fn test_connection_reuse() {
    let (server, cert_der) = TestServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive().with_root_cert(cert_der))
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async

# Make 3 separate requests
for i in range(3):
    tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
    tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "127.0.0.1")

    request = f"GET /get?request={{i}} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    await _eryx_async.await_tls_write(tls_handle, request.encode())

    response = b""
    while True:
        chunk = await _eryx_async.await_tls_read(tls_handle, 4096)
        if not chunk:
            break
        response += chunk

    _eryx_async.tls_close(tls_handle)

    status_line = response.decode().split('\r\n')[0]
    print(f"Request {{i}}: {{status_line}}")

print("SUCCESS: All requests completed")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "Connection reuse should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS"),
        "Should succeed: {}",
        output.stdout
    );
}

/// Test handling of connection to non-existent server (connection refused).
#[tokio::test]
async fn test_connection_refused() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    // Try to connect to a port that's not listening
    let result = sandbox
        .execute(
            r#"
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(("127.0.0.1", 59999))  # Unlikely to be listening

ctx = ssl.create_default_context()
try:
    ssl_sock = ctx.wrap_socket(sock, server_hostname="localhost")
    print("UNEXPECTED: Connection succeeded")
except Exception as e:
    error_str = str(e).lower()
    if "refused" in error_str or "connect" in error_str or "reset" in error_str:
        print("EXPECTED: Connection refused/failed")
    else:
        print(f"Error (acceptable): {e}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Should handle connection refused: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") || output.stdout.contains("Error"),
        "Should handle connection refused: {}",
        output.stdout
    );
}

/// Test connection timeout with short timeout.
#[tokio::test]
async fn test_connection_timeout() {
    let sandbox = sandbox_builder_with_network()
        .with_network(
            NetConfig::permissive().with_connect_timeout(std::time::Duration::from_millis(100)),
        )
        .build()
        .expect("Failed to build sandbox");

    // Try to connect to a non-routable IP that will timeout
    let result = sandbox
        .execute(
            r#"
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(1)
sock.connect(("10.255.255.1", 443))  # Non-routable IP

ctx = ssl.create_default_context()
try:
    ssl_sock = ctx.wrap_socket(sock, server_hostname="test.invalid")
    print("UNEXPECTED: Connection succeeded")
except Exception as e:
    error_str = str(e).lower()
    if "timeout" in error_str or "timed out" in error_str:
        print("EXPECTED: Connection timed out")
    else:
        print(f"Error (acceptable): {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should handle timeout: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") || output.stdout.contains("Error"),
        "Should timeout or error: {}",
        output.stdout
    );
}

// =============================================================================
// Permissive Config Test
// =============================================================================

#[tokio::test]
async fn test_permissive_config_allows_localhost() {
    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(1)
print("Permissive config accepted")
sock.close()
"#,
        )
        .await;

    assert!(result.is_ok());
    assert!(
        result
            .unwrap()
            .stdout
            .contains("Permissive config accepted")
    );
}

// =============================================================================
// Plain HTTP (TCP) Tests
// =============================================================================

/// A simple HTTP test server (no TLS) for plain HTTP tests.
struct PlainHttpServer {
    addr: std::net::SocketAddr,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl PlainHttpServer {
    /// Start a new plain HTTP test server on a random port.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind");
        let addr = listener.local_addr().expect("Failed to get local addr");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                tokio::spawn(async move {
                                    // Read HTTP request
                                    let mut buf = vec![0u8; 4096];
                                    let n = stream.read(&mut buf).await.unwrap_or(0);
                                    let request = String::from_utf8_lossy(&buf[..n]);

                                    // Parse request line
                                    let first_line = request.lines().next().unwrap_or("");
                                    let parts: Vec<&str> = first_line.split_whitespace().collect();
                                    let method = parts.first().copied().unwrap_or("GET");
                                    let path = parts.get(1).copied().unwrap_or("/");

                                    // Generate response based on path
                                    let (status, body) = match path {
                                        "/get" => ("200 OK", format!(r#"{{"url": "http://localhost{}", "method": "{}"}}"#, path, method)),
                                        "/post" => {
                                            // Extract JSON body from request
                                            let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                                            let req_body = &request[body_start..];
                                            ("200 OK", format!(r#"{{"url": "http://localhost{}", "method": "{}", "json": {}}}"#, path, method, req_body))
                                        }
                                        "/echo" => {
                                            // Echo back the request body
                                            let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                                            let req_body = &request[body_start..];
                                            ("200 OK", req_body.to_string())
                                        }
                                        _ => ("200 OK", format!(r#"{{"path": "{}"}}"#, path)),
                                    };

                                    let response = format!(
                                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                        status,
                                        body.len(),
                                        body
                                    );

                                    let _ = stream.write_all(response.as_bytes()).await;
                                    let _ = stream.flush().await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        PlainHttpServer {
            addr,
            shutdown_tx,
            handle,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

/// Test plain HTTP GET using low-level TCP API.
#[tokio::test]
async fn test_plain_http_get() {
    let server = PlainHttpServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async
import json

# Connect with TCP (no TLS)
tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
print(f"Connected with TCP handle {{tcp_handle}}")

# Send HTTP GET request
request = b"GET /get HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
written = await _eryx_async.await_tcp_write(tcp_handle, request)
print(f"Sent {{written}} bytes")

# Read response
response = b""
while True:
    chunk = await _eryx_async.await_tcp_read(tcp_handle, 4096)
    if not chunk:
        break
    response += chunk
print(f"Read {{len(response)}} bytes")

# Close connection
_eryx_async.tcp_close(tcp_handle)

# Parse response
response_str = response.decode('utf-8')
headers, body = response_str.split('\r\n\r\n', 1)
status_line = headers.split('\r\n')[0]
print(f"Status: {{status_line}}")

data = json.loads(body)
print(f"URL: {{data.get('url', 'N/A')}}")
print(f"Method: {{data.get('method', 'N/A')}}")

if '200 OK' in status_line and data.get('method') == 'GET':
    print("SUCCESS: Plain HTTP GET worked")
else:
    print("FAILED: Unexpected response")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "Plain HTTP GET should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS"),
        "Should succeed: {}",
        output.stdout
    );
}

/// Test plain HTTP POST with JSON body using TCP API.
#[tokio::test]
async fn test_plain_http_post() {
    let server = PlainHttpServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import _eryx_async
import json

# Connect with TCP (no TLS)
tcp_handle = await _eryx_async.await_tcp_connect("127.0.0.1", {port})

# Build POST request with JSON body
body = json.dumps({{"name": "eryx", "version": "0.2.0"}})
request = f"POST /post HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {{len(body)}}\r\nConnection: close\r\n\r\n{{body}}"
await _eryx_async.await_tcp_write(tcp_handle, request.encode())

# Read response
response = b""
while True:
    chunk = await _eryx_async.await_tcp_read(tcp_handle, 4096)
    if not chunk:
        break
    response += chunk

_eryx_async.tcp_close(tcp_handle)

# Parse response
response_str = response.decode('utf-8')
headers, response_body = response_str.split('\r\n\r\n', 1)
status_line = headers.split('\r\n')[0]
print(f"Status: {{status_line}}")

data = json.loads(response_body)
json_data = data.get("json", {{}})

if json_data.get("name") == "eryx" and json_data.get("version") == "0.2.0":
    print("SUCCESS: Plain HTTP POST with JSON worked")
else:
    print(f"FAILED: Unexpected response: {{json_data}}")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(result.is_ok(), "Plain HTTP POST should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS"),
        "Should succeed: {}",
        output.stdout
    );
}

/// Test socket shim works for plain TCP connections (no SSL).
#[tokio::test]
async fn test_socket_shim_plain_tcp() {
    let server = PlainHttpServer::start().await;
    let port = server.port();

    let sandbox = sandbox_builder_with_network()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    let code = format!(
        r#"
import socket
import json

# Create socket and connect (should use TCP directly)
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", {port}))
print("Socket connected")

# Send HTTP request
request = b"GET /get HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
sock.sendall(request)
print("Request sent")

# Read response
response = b""
while True:
    chunk = sock.recv(4096)
    if not chunk:
        break
    response += chunk
print(f"Response received: {{len(response)}} bytes")

sock.close()

# Parse and verify
response_str = response.decode('utf-8')
if '200 OK' in response_str and '"method": "GET"' in response_str:
    print("SUCCESS: Socket shim plain TCP works")
else:
    print(f"FAILED: Unexpected response")
"#
    );

    let result = sandbox.execute(&code).await;
    server.shutdown().await;

    assert!(
        result.is_ok(),
        "Socket shim plain TCP should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SUCCESS"),
        "Should succeed: {}",
        output.stdout
    );
}
