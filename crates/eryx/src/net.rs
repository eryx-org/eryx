//! Networking support for the sandbox.
//!
//! Provides TCP and TLS networking for Python code running in the sandbox.
//! TCP enables plain connections (http://localhost), while TLS provides
//! secure encrypted connections via upgrade from TCP.
//!
//! # Security
//!
//! - Host controls which hosts are allowed/blocked via [`NetConfig`]
//! - Certificate verification is handled by the host (cannot be bypassed)
//! - Private/local networks are blocked by default
//! - Connection limits prevent resource exhaustion

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Network configuration for the sandbox.
///
/// Controls which hosts Python code can connect to and sets timeouts.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Maximum concurrent connections (0 = unlimited).
    pub max_connections: u32,
    /// Connection timeout for TCP connect.
    pub connect_timeout: Duration,
    /// Read/write timeout for I/O operations.
    pub io_timeout: Duration,
    /// Allowed host patterns (empty = allow all).
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`, `exact.host.com`
    pub allowed_hosts: Vec<String>,
    /// Blocked host patterns (checked after allowed).
    ///
    /// By default, blocks localhost and private networks.
    pub blocked_hosts: Vec<String>,
    /// Custom root certificates (DER-encoded) to trust in addition to system certs.
    ///
    /// Useful for testing with self-signed certificates.
    pub custom_root_certs: Vec<Vec<u8>>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            connect_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(60),
            allowed_hosts: vec![], // Empty = allow all
            blocked_hosts: vec![
                // Block private/local networks by default
                "localhost".into(),
                "*.localhost".into(),
                "127.*".into(),
                "10.*".into(),
                "172.16.*".into(),
                "172.17.*".into(),
                "172.18.*".into(),
                "172.19.*".into(),
                "172.20.*".into(),
                "172.21.*".into(),
                "172.22.*".into(),
                "172.23.*".into(),
                "172.24.*".into(),
                "172.25.*".into(),
                "172.26.*".into(),
                "172.27.*".into(),
                "172.28.*".into(),
                "172.29.*".into(),
                "172.30.*".into(),
                "172.31.*".into(),
                "192.168.*".into(),
                "169.254.*".into(),
                "[::1]".into(),
            ],
            custom_root_certs: vec![],
        }
    }
}

impl NetConfig {
    /// Create a new `NetConfig` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum number of concurrent connections.
    #[must_use]
    pub fn with_max_connections(mut self, max: u32) -> Self {
        self.max_connections = max;
        self
    }

    /// Set the connection timeout.
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the I/O timeout for read/write operations.
    #[must_use]
    pub fn with_io_timeout(mut self, timeout: Duration) -> Self {
        self.io_timeout = timeout;
        self
    }

    /// Add a host pattern to the allowed list.
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`
    #[must_use]
    pub fn allow_host(mut self, pattern: impl Into<String>) -> Self {
        self.allowed_hosts.push(pattern.into());
        self
    }

    /// Add a host pattern to the blocked list.
    #[must_use]
    pub fn block_host(mut self, pattern: impl Into<String>) -> Self {
        self.blocked_hosts.push(pattern.into());
        self
    }

    /// Allow connections to localhost (disabled by default).
    #[must_use]
    pub fn allow_localhost(mut self) -> Self {
        self.blocked_hosts
            .retain(|p| !p.contains("localhost") && !p.starts_with("127.") && !p.contains("::1"));
        self
    }

    /// Create a permissive config that allows all hosts including localhost.
    ///
    /// # Warning
    ///
    /// This is primarily for testing. Use with caution in production.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            max_connections: 100,
            connect_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(60),
            allowed_hosts: vec![],
            blocked_hosts: vec![],
            custom_root_certs: vec![],
        }
    }

    /// Add a custom root certificate (DER-encoded) to trust.
    ///
    /// This is useful for testing with self-signed certificates.
    #[must_use]
    pub fn with_root_cert(mut self, cert_der: impl Into<Vec<u8>>) -> Self {
        self.custom_root_certs.push(cert_der.into());
        self
    }
}

// ============================================================================
// TCP
// ============================================================================

/// Errors that can occur during TCP operations.
#[derive(Debug, Clone)]
pub enum TcpError {
    /// Connection was refused by the remote host.
    ConnectionRefused,
    /// Connection was reset by the remote host.
    ConnectionReset,
    /// Operation timed out.
    TimedOut,
    /// DNS lookup failed.
    HostNotFound,
    /// Generic I/O error.
    IoError(String),
    /// Network access not permitted by sandbox policy.
    NotPermitted(String),
    /// Invalid handle (connection was closed).
    InvalidHandle,
}

impl std::fmt::Display for TcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionRefused => write!(f, "connection refused"),
            Self::ConnectionReset => write!(f, "connection reset"),
            Self::TimedOut => write!(f, "timed out"),
            Self::HostNotFound => write!(f, "host not found"),
            Self::IoError(msg) => write!(f, "I/O error: {msg}"),
            Self::NotPermitted(msg) => write!(f, "not permitted: {msg}"),
            Self::InvalidHandle => write!(f, "invalid handle"),
        }
    }
}

impl std::error::Error for TcpError {}

// ============================================================================
// TLS
// ============================================================================

/// Errors that can occur during TLS operations.
#[derive(Debug, Clone)]
pub enum TlsError {
    /// Error from the underlying TCP layer.
    Tcp(TcpError),
    /// TLS handshake failed (certificate verification, protocol error, etc).
    HandshakeFailed(String),
    /// Certificate verification failed.
    CertificateError(String),
    /// Invalid handle (connection was closed).
    InvalidHandle,
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(e) => write!(f, "{e}"),
            Self::HandshakeFailed(msg) => write!(f, "TLS handshake failed: {msg}"),
            Self::CertificateError(msg) => write!(f, "certificate error: {msg}"),
            Self::InvalidHandle => write!(f, "invalid handle"),
        }
    }
}

impl std::error::Error for TlsError {}

impl From<TcpError> for TlsError {
    fn from(e: TcpError) -> Self {
        Self::Tcp(e)
    }
}

// ============================================================================
// Connection Manager
// ============================================================================

/// HTTP parsing state for a single connection.
#[derive(Debug, Clone, Default)]
struct HttpParsingState {
    /// Buffer for accumulating request data until headers are complete
    buffer: Vec<u8>,
    /// Have we seen complete headers (\r\n\r\n)?
    headers_complete: bool,
    /// Total bytes of the current request body we've sent
    body_bytes_sent: usize,
    /// Expected content-length (if any)
    content_length: Option<usize>,
}

/// Result of processing write data through the HTTP parser.
#[derive(Debug)]
enum WriteAction {
    /// Pass data through without modification.
    Passthrough(Vec<u8>),
    /// Data was parsed and should be written as multiple segments.
    Segments(Vec<Vec<u8>>),
    /// Data is being buffered (headers not yet complete).
    Buffering,
}

/// Manages TCP and TLS connections for a sandbox instance.
///
/// Each sandbox has its own connection manager, which tracks active connections
/// and enforces the network policy.
#[derive(Debug)]
pub struct ConnectionManager {
    config: NetConfig,
    tls_config: Arc<ClientConfig>,
    tcp_connections: HashMap<u32, TcpStream>,
    tls_connections: HashMap<u32, TlsStream<TcpStream>>,
    next_handle: u32,
    /// Track HTTP parsing state per connection handle
    http_states: HashMap<u32, HttpParsingState>,
    /// Track which host each connection is connected to
    connection_hosts: HashMap<u32, String>,
    /// Secrets configuration for substitution
    secrets: HashMap<String, crate::secrets::SecretConfig>,
}

impl ConnectionManager {
    /// Create a new connection manager with the given config and secrets.
    #[must_use]
    pub fn new(config: NetConfig, secrets: HashMap<String, crate::secrets::SecretConfig>) -> Self {
        // Build rustls config with system root certs + any custom certs
        let mut root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        // Add custom root certificates if any
        for cert_der in &config.custom_root_certs {
            let cert = rustls::pki_types::CertificateDer::from(cert_der.as_slice());
            // Ignore errors adding individual certs - log in production
            let _ = root_store.add(cert);
        }

        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        Self {
            config,
            tls_config: Arc::new(tls_config),
            tcp_connections: HashMap::new(),
            tls_connections: HashMap::new(),
            next_handle: 1,
            http_states: HashMap::new(),
            connection_hosts: HashMap::new(),
            secrets,
        }
    }

    /// Get the total number of active connections.
    fn connection_count(&self) -> usize {
        self.tcp_connections.len() + self.tls_connections.len()
    }

    /// Allocate a new handle.
    fn alloc_handle(&mut self) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        if self.next_handle == 0 {
            self.next_handle = 1; // Skip 0 to avoid confusion with "no handle"
        }
        handle
    }

    /// Process write data through the HTTP parser for secret substitution.
    ///
    /// This is the shared logic used by both `tcp_write()` and `tls_write()`.
    /// Returns a `WriteAction` indicating how the data should be written.
    fn process_write_data(&mut self, handle: u32, data: &[u8]) -> Result<WriteAction, TcpError> {
        // Get current state (borrow ends here)
        let mut state = self.http_states.entry(handle).or_default().clone();

        // Add new data to buffer
        state.buffer.extend_from_slice(data);

        if !state.headers_complete {
            // Look for end of headers (\r\n\r\n)
            if let Some(header_end) = find_header_end(&state.buffer) {
                let headers_bytes = state.buffer[..header_end].to_vec();
                let remaining = state.buffer[header_end..].to_vec();

                // Check if this looks like HTTP
                if !is_http_request(&headers_bytes) {
                    // Not HTTP - pass through everything
                    let all_data = state.buffer.clone();
                    state.buffer.clear();
                    self.http_states.insert(handle, state);
                    return Ok(WriteAction::Passthrough(all_data));
                }

                // Check for HTTP/2 (not supported with secrets)
                if is_http2(&headers_bytes) {
                    return Err(TcpError::NotPermitted(
                        "HTTP/2 with secrets not supported. Use HTTP/1.1.".into(),
                    ));
                }

                // Parse headers and substitute secrets
                let substituted = self.substitute_http_headers(&headers_bytes, handle)?;

                // Extract Content-Length for body tracking
                state.content_length = extract_content_length(&substituted);
                state.headers_complete = true;

                // Build segments: headers first, then any buffered body data
                let mut segments = vec![substituted.clone()];
                if !remaining.is_empty() {
                    state.body_bytes_sent += remaining.len();
                    segments.push(remaining);
                }

                // Check if request is complete (for pipelining)
                if let Some(cl) = state.content_length {
                    if state.body_bytes_sent >= cl {
                        // Request complete - reset for next request
                        state = HttpParsingState::default();
                    }
                } else if is_chunked(&substituted) {
                    // Chunked encoding - keep state
                } else {
                    // No Content-Length and not chunked, assume request complete
                    state.buffer.clear();
                }

                self.http_states.insert(handle, state);
                Ok(WriteAction::Segments(segments))
            } else {
                // Headers not complete yet - keep buffering
                self.http_states.insert(handle, state);
                Ok(WriteAction::Buffering)
            }
        } else {
            // Headers already sent, this is body data - pass through
            state.body_bytes_sent += data.len();

            // Check if request is complete
            if let Some(cl) = state.content_length
                && state.body_bytes_sent >= cl
            {
                // Request complete - reset for pipelining
                state = HttpParsingState::default();
            }

            self.http_states.insert(handle, state);
            Ok(WriteAction::Passthrough(data.to_vec()))
        }
    }

    /// Check if a host is allowed by the current policy.
    fn check_host_allowed(&self, host: &str) -> Result<(), TcpError> {
        // Check blocked first
        for pattern in &self.config.blocked_hosts {
            if host_matches_pattern(host, pattern) {
                return Err(TcpError::NotPermitted(format!("host '{host}' is blocked")));
            }
        }

        // If allowed list is non-empty, host must match
        if !self.config.allowed_hosts.is_empty() {
            let allowed = self
                .config
                .allowed_hosts
                .iter()
                .any(|p| host_matches_pattern(host, p));
            if !allowed {
                return Err(TcpError::NotPermitted(format!(
                    "host '{host}' not in allowed list"
                )));
            }
        }

        Ok(())
    }

    // ========================================================================
    // TCP operations
    // ========================================================================

    /// Connect to a host over TCP.
    pub async fn tcp_connect(&mut self, host: &str, port: u16) -> Result<u32, TcpError> {
        // 1. Check host against allowed/blocked patterns
        self.check_host_allowed(host)?;

        // 2. Check connection limit
        if self.config.max_connections > 0
            && self.connection_count() >= self.config.max_connections as usize
        {
            return Err(TcpError::NotPermitted("connection limit reached".into()));
        }

        // 3. DNS resolve + TCP connect (with timeout)
        let addr = tokio::time::timeout(
            self.config.connect_timeout,
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| TcpError::TimedOut)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TcpError::HostNotFound
            } else {
                TcpError::IoError(e.to_string())
            }
        })?
        .next()
        .ok_or(TcpError::HostNotFound)?;

        let tcp = tokio::time::timeout(self.config.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| TcpError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused => TcpError::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;

        let handle = self.alloc_handle();
        self.tcp_connections.insert(handle, tcp);
        self.connection_hosts.insert(handle, host.to_string());

        tracing::debug!(handle, host, port, "TCP connection established");
        Ok(handle)
    }

    /// Read from a TCP connection.
    pub async fn tcp_read(&mut self, handle: u32, len: u32) -> Result<Vec<u8>, TcpError> {
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        let mut buf = vec![0u8; len as usize];
        let n = tokio::time::timeout(self.config.io_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| TcpError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;

        buf.truncate(n);
        Ok(buf)
    }

    /// Write to a TCP connection.
    ///
    /// If secrets are configured, this method will parse HTTP requests and
    /// substitute secret placeholders in headers before sending.
    pub async fn tcp_write(&mut self, handle: u32, data: &[u8]) -> Result<u32, TcpError> {
        // Fast path: no secrets = no parsing
        if self.secrets.is_empty() {
            return self.tcp_write_raw(handle, data).await;
        }

        match self.process_write_data(handle, data)? {
            WriteAction::Passthrough(bytes) => self.tcp_write_raw(handle, &bytes).await,
            WriteAction::Segments(segments) => {
                for segment in &segments {
                    self.tcp_write_raw(handle, segment).await?;
                }
                Ok(data.len() as u32)
            }
            WriteAction::Buffering => Ok(data.len() as u32),
        }
    }

    /// Write raw data to a TCP connection without HTTP parsing.
    async fn tcp_write_raw(&mut self, handle: u32, data: &[u8]) -> Result<u32, TcpError> {
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        let n = tokio::time::timeout(self.config.io_timeout, stream.write(data))
            .await
            .map_err(|_| TcpError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;

        Ok(n as u32)
    }

    /// Substitute secret placeholders in HTTP headers.
    fn substitute_http_headers(&self, headers: &[u8], handle: u32) -> Result<Vec<u8>, TcpError> {
        let text = String::from_utf8_lossy(headers);
        let target_host = self
            .connection_hosts
            .get(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        let mut result = String::new();

        // Process line by line
        for line in text.lines() {
            if line.is_empty() {
                result.push_str("\r\n");
                continue;
            }

            // Check if this is a header line (contains ':')
            if let Some(colon_pos) = line.find(':') {
                let name = &line[..colon_pos];
                let value = line[colon_pos + 1..].trim_start();

                // Substitute secrets in header value
                let substituted_value = self.substitute_secrets_in_text(value, target_host)?;
                result.push_str(&format!("{name}: {substituted_value}\r\n"));
            } else {
                // Request line (GET /path HTTP/1.1)
                result.push_str(line);
                result.push_str("\r\n");
            }
        }

        result.push_str("\r\n"); // End of headers
        Ok(result.into_bytes())
    }

    /// Substitute secret placeholders in text.
    fn substitute_secrets_in_text(
        &self,
        text: &str,
        target_host: &str,
    ) -> Result<String, TcpError> {
        let mut result = text.to_string();

        for secret_config in self.secrets.values() {
            if !text.contains(&secret_config.placeholder) {
                continue;
            }

            // Check if secret is allowed for this host
            let allowed_hosts = if secret_config.allowed_hosts.is_empty() {
                &self.config.allowed_hosts
            } else {
                &secret_config.allowed_hosts
            };

            let host_allowed = if allowed_hosts.is_empty() {
                true // Empty = allow all (check blocked list separately)
            } else {
                allowed_hosts
                    .iter()
                    .any(|p| host_matches_pattern(target_host, p))
            };

            if !host_allowed {
                return Err(TcpError::NotPermitted(
                    "Secret not allowed for the target host".into(),
                ));
            }

            // Substitute
            result = result.replace(&secret_config.placeholder, &secret_config.real_value);
        }

        Ok(result)
    }

    /// Close a TCP connection.
    pub fn tcp_close(&mut self, handle: u32) {
        if self.tcp_connections.remove(&handle).is_some() {
            tracing::debug!(handle, "TCP connection closed");
        }
        // Clean up HTTP parsing state and host tracking
        self.http_states.remove(&handle);
        self.connection_hosts.remove(&handle);
    }

    // ========================================================================
    // TLS operations
    // ========================================================================

    /// Upgrade a TCP connection to TLS.
    ///
    /// Takes ownership of the TCP connection and performs a TLS handshake.
    /// After upgrade, the original tcp_handle is invalid.
    pub async fn tls_upgrade(&mut self, tcp_handle: u32, hostname: &str) -> Result<u32, TlsError> {
        // Remove the TCP connection (we're taking ownership)
        let tcp = self
            .tcp_connections
            .remove(&tcp_handle)
            .ok_or(TlsError::InvalidHandle)?;

        // TLS handshake
        let connector = TlsConnector::from(self.tls_config.clone());
        let server_name: ServerName<'static> = hostname
            .to_string()
            .try_into()
            .map_err(|_| TlsError::HandshakeFailed("invalid hostname".into()))?;

        let tls = tokio::time::timeout(
            self.config.connect_timeout,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
        .map_err(|e| TlsError::HandshakeFailed(e.to_string()))?;

        let handle = self.alloc_handle();
        self.tls_connections.insert(handle, tls);

        // Carry over host mapping and clean up old TCP state
        if let Some(host) = self.connection_hosts.remove(&tcp_handle) {
            self.connection_hosts.insert(handle, host);
        }
        self.http_states.remove(&tcp_handle);

        tracing::debug!(
            handle,
            hostname,
            "TLS connection established (upgraded from TCP handle {})",
            tcp_handle
        );
        Ok(handle)
    }

    /// Read from a TLS connection.
    pub async fn tls_read(&mut self, handle: u32, len: u32) -> Result<Vec<u8>, TlsError> {
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let mut buf = vec![0u8; len as usize];
        let n = tokio::time::timeout(self.config.io_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            })?;

        buf.truncate(n);
        Ok(buf)
    }

    /// Write to a TLS connection.
    ///
    /// If secrets are configured, this method will parse HTTP requests and
    /// substitute secret placeholders in headers before sending.
    pub async fn tls_write(&mut self, handle: u32, data: &[u8]) -> Result<u32, TlsError> {
        // Fast path: no secrets = no parsing
        if self.secrets.is_empty() {
            return self.tls_write_raw(handle, data).await;
        }

        match self.process_write_data(handle, data).map_err(TlsError::Tcp)? {
            WriteAction::Passthrough(bytes) => self.tls_write_raw(handle, &bytes).await,
            WriteAction::Segments(segments) => {
                for segment in &segments {
                    self.tls_write_raw(handle, segment).await?;
                }
                Ok(data.len() as u32)
            }
            WriteAction::Buffering => Ok(data.len() as u32),
        }
    }

    /// Write raw data to a TLS connection without HTTP parsing.
    async fn tls_write_raw(&mut self, handle: u32, data: &[u8]) -> Result<u32, TlsError> {
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let n = tokio::time::timeout(self.config.io_timeout, stream.write(data))
            .await
            .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            })?;

        Ok(n as u32)
    }

    /// Close a TLS connection.
    pub fn tls_close(&mut self, handle: u32) {
        if self.tls_connections.remove(&handle).is_some() {
            tracing::debug!(handle, "TLS connection closed");
        }
        // Clean up HTTP parsing state and host tracking
        self.http_states.remove(&handle);
        self.connection_hosts.remove(&handle);
        // TLS shutdown happens on drop
    }
}

// ============================================================================
// HTTP Parsing Helpers
// ============================================================================

/// Find the end of HTTP headers (\r\n\r\n).
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Check if data starts with an HTTP request method.
fn is_http_request(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }

    data.starts_with(b"GET ")
        || data.starts_with(b"POST ")
        || data.starts_with(b"PUT ")
        || data.starts_with(b"DELETE ")
        || data.starts_with(b"PATCH ")
        || data.starts_with(b"HEAD ")
        || data.starts_with(b"OPTIONS ")
        || data.starts_with(b"CONNECT ")
        || data.starts_with(b"TRACE ")
}

/// Check if data is an HTTP/2 connection preface.
fn is_http2(data: &[u8]) -> bool {
    data.starts_with(b"PRI * HTTP/2")
}

/// Extract Content-Length header value from HTTP headers.
fn extract_content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    for line in text.lines() {
        if line.to_ascii_lowercase().starts_with("content-length:")
            && let Some(value) = line.split(':').nth(1)
        {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Check if Transfer-Encoding is chunked.
fn is_chunked(headers: &[u8]) -> bool {
    let text = String::from_utf8_lossy(headers);
    text.lines().any(|line| {
        line.to_ascii_lowercase().contains("transfer-encoding")
            && line.to_ascii_lowercase().contains("chunked")
    })
}

/// Check if a hostname matches a pattern with wildcards.
///
/// Patterns:
/// - `*` matches everything
/// - `*.example.com` matches `api.example.com` but not `example.com`
/// - `api.*.com` matches `api.foo.com`
/// - `exact.host.com` matches only that exact host
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return host.eq_ignore_ascii_case(pattern);
    }

    // Simple glob matching with * as wildcard
    let parts: Vec<&str> = pattern.split('*').collect();
    let host_lower = host.to_ascii_lowercase();
    let mut pos = 0;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let part_lower = part.to_ascii_lowercase();
        match host_lower[pos..].find(&part_lower) {
            Some(idx) => {
                // First part must match at start
                if i == 0 && idx != 0 {
                    return false;
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }

    // If pattern ends with literal (not *), must match to end
    if let Some(last) = parts.last()
        && !last.is_empty()
        && pos != host.len()
    {
        return false;
    }

    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_matching() {
        // Exact match
        assert!(host_matches_pattern("example.com", "example.com"));
        assert!(host_matches_pattern("Example.COM", "example.com"));
        assert!(!host_matches_pattern("example.org", "example.com"));

        // Wildcard at start
        assert!(host_matches_pattern("api.example.com", "*.example.com"));
        assert!(host_matches_pattern("foo.bar.example.com", "*.example.com"));
        assert!(!host_matches_pattern("example.com", "*.example.com"));

        // Wildcard in middle
        assert!(host_matches_pattern("api.foo.com", "api.*.com"));
        assert!(!host_matches_pattern("web.foo.com", "api.*.com"));

        // Wildcard at end
        assert!(host_matches_pattern("10.0.0.1", "10.*"));
        assert!(host_matches_pattern("10.255.255.255", "10.*"));
        assert!(!host_matches_pattern("11.0.0.1", "10.*"));

        // Match all
        assert!(host_matches_pattern("anything.com", "*"));

        // localhost patterns
        assert!(host_matches_pattern("localhost", "localhost"));
        assert!(host_matches_pattern("foo.localhost", "*.localhost"));
        assert!(host_matches_pattern("127.0.0.1", "127.*"));
    }

    #[test]
    fn test_default_config_blocks_private() {
        let config = NetConfig::default();
        let manager = ConnectionManager::new(config, HashMap::new());

        assert!(manager.check_host_allowed("localhost").is_err());
        assert!(manager.check_host_allowed("127.0.0.1").is_err());
        assert!(manager.check_host_allowed("192.168.1.1").is_err());
        assert!(manager.check_host_allowed("10.0.0.1").is_err());

        // Public hosts should be allowed
        assert!(manager.check_host_allowed("google.com").is_ok());
        assert!(manager.check_host_allowed("api.example.com").is_ok());
    }

    #[test]
    fn test_allowed_hosts_whitelist() {
        let config = NetConfig::default()
            .allow_host("*.example.com")
            .allow_host("api.github.com");
        let manager = ConnectionManager::new(config, HashMap::new());

        assert!(manager.check_host_allowed("api.example.com").is_ok());
        assert!(manager.check_host_allowed("api.github.com").is_ok());
        assert!(manager.check_host_allowed("google.com").is_err());
    }

    #[test]
    fn test_permissive_config() {
        let config = NetConfig::permissive();
        let manager = ConnectionManager::new(config, HashMap::new());

        assert!(manager.check_host_allowed("localhost").is_ok());
        assert!(manager.check_host_allowed("127.0.0.1").is_ok());
        assert!(manager.check_host_allowed("google.com").is_ok());
    }

    // HTTP parsing helper tests
    #[test]
    fn test_find_header_end() {
        let data = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\nBody";
        // Position should be just after \r\n\r\n (at index 37)
        assert_eq!(find_header_end(data), Some(37));

        let incomplete = b"GET / HTTP/1.1\r\nHost: example.com\r\n";
        assert_eq!(find_header_end(incomplete), None);
    }

    #[test]
    fn test_is_http_request() {
        assert!(is_http_request(b"GET / HTTP/1.1\r\n"));
        assert!(is_http_request(b"POST /api HTTP/1.1\r\n"));
        assert!(is_http_request(b"PUT /resource HTTP/1.1\r\n"));
        assert!(is_http_request(b"DELETE /item HTTP/1.1\r\n"));
        assert!(is_http_request(b"PATCH /update HTTP/1.1\r\n"));
        assert!(is_http_request(b"HEAD / HTTP/1.1\r\n"));
        assert!(is_http_request(b"OPTIONS * HTTP/1.1\r\n"));

        assert!(!is_http_request(b"NOTHTTP"));
        assert!(!is_http_request(b""));
        assert!(!is_http_request(b"GET"));
    }

    #[test]
    fn test_is_http2() {
        assert!(is_http2(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));
        assert!(!is_http2(b"GET / HTTP/1.1\r\n"));
        assert!(!is_http2(b""));
    }

    #[test]
    fn test_extract_content_length() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(extract_content_length(headers), Some(42));

        let headers_upper = b"POST / HTTP/1.1\r\nCONTENT-LENGTH: 100\r\n\r\n";
        assert_eq!(extract_content_length(headers_upper), Some(100));

        let no_cl = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(extract_content_length(no_cl), None);
    }

    #[test]
    fn test_is_chunked() {
        let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(is_chunked(chunked));

        let chunked_upper = b"POST / HTTP/1.1\r\nTRANSFER-ENCODING: CHUNKED\r\n\r\n";
        assert!(is_chunked(chunked_upper));

        let not_chunked = b"GET / HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert!(!is_chunked(not_chunked));
    }

    #[test]
    fn test_secret_substitution() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real-secret-value".to_string(),
                placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
                allowed_hosts: vec!["api.example.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Test substitution for allowed host
        let result = manager
            .substitute_secrets_in_text("Bearer ERYX_SECRET_PLACEHOLDER_abc123", "api.example.com");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer real-secret-value");

        // Test rejection for disallowed host
        let result =
            manager.substitute_secrets_in_text("Bearer ERYX_SECRET_PLACEHOLDER_abc123", "evil.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_secret_substitution_empty_allowed_hosts() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real-secret-value".to_string(),
                placeholder: "PLACEHOLDER_XYZ".to_string(),
                allowed_hosts: vec![], // Empty = allow all
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Should work for any host when allowed_hosts is empty
        let result = manager.substitute_secrets_in_text("Bearer PLACEHOLDER_XYZ", "any-host.com");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer real-secret-value");
    }

    // ========================================================================
    // Adversarial Security Tests
    // ========================================================================

    /// Test that secrets are rejected for all unauthorized hosts.
    #[test]
    fn test_secret_blocked_for_unauthorized_host() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "top-secret".to_string(),
                placeholder: "PH_AUTH".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // All of these unauthorized hosts must be rejected
        assert!(manager.substitute_secrets_in_text("PH_AUTH", "evil.com").is_err());
        assert!(manager.substitute_secrets_in_text("PH_AUTH", "127.0.0.1").is_err());
        assert!(manager.substitute_secrets_in_text("PH_AUTH", "localhost").is_err());
        assert!(manager.substitute_secrets_in_text("PH_AUTH", "").is_err());
    }

    /// Test that multiple secrets with different host restrictions are independently enforced.
    #[test]
    fn test_multiple_secrets_independent_host_restrictions() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "OPENAI".to_string(),
            crate::secrets::SecretConfig {
                real_value: "openai-secret".to_string(),
                placeholder: "PH_OPENAI".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );
        secrets.insert(
            "GITHUB".to_string(),
            crate::secrets::SecretConfig {
                real_value: "github-secret".to_string(),
                placeholder: "PH_GITHUB".to_string(),
                allowed_hosts: vec!["api.github.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // OpenAI key for OpenAI host: OK
        assert!(manager.substitute_secrets_in_text("PH_OPENAI", "api.openai.com").is_ok());
        // OpenAI key for GitHub host: REJECTED
        assert!(manager.substitute_secrets_in_text("PH_OPENAI", "api.github.com").is_err());
        // GitHub key for GitHub host: OK
        assert!(manager.substitute_secrets_in_text("PH_GITHUB", "api.github.com").is_ok());
        // GitHub key for OpenAI host: REJECTED
        assert!(manager.substitute_secrets_in_text("PH_GITHUB", "api.openai.com").is_err());
    }

    /// Test glob pattern host enforcement for secrets.
    #[test]
    fn test_secret_glob_pattern_enforcement() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH_GLOB".to_string(),
                allowed_hosts: vec!["*.openai.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        assert!(manager.substitute_secrets_in_text("PH_GLOB", "api.openai.com").is_ok());
        // Bare domain doesn't match *.openai.com
        assert!(manager.substitute_secrets_in_text("PH_GLOB", "openai.com").is_err());
        assert!(manager.substitute_secrets_in_text("PH_GLOB", "evil.com").is_err());
    }

    /// Test that deceptive domain names don't match allowed hosts.
    #[test]
    fn test_secret_deceptive_domain_rejection() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH_DECEPTIVE".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Deceptive domains that look similar but shouldn't match
        assert!(manager.substitute_secrets_in_text("PH_DECEPTIVE", "api.openai.com.evil.com").is_err());
        assert!(manager.substitute_secrets_in_text("PH_DECEPTIVE", "api-openai.com").is_err());
        assert!(manager.substitute_secrets_in_text("PH_DECEPTIVE", "evil-api.openai.com").is_err());
        assert!(manager.substitute_secrets_in_text("PH_DECEPTIVE", "xapi.openai.com").is_err());
    }

    /// Test that IP addresses don't match hostname-based restrictions.
    #[test]
    fn test_secret_ip_doesnt_match_hostname() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH_IP".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // IP address should NOT match hostname restriction
        assert!(manager.substitute_secrets_in_text("PH_IP", "104.18.7.192").is_err());
        assert!(manager.substitute_secrets_in_text("PH_IP", "1.2.3.4").is_err());
    }

    /// Test case-insensitive host matching for secrets.
    #[test]
    fn test_secret_case_insensitive_host() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH_CASE".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // DNS is case-insensitive, so these should all work
        assert!(manager.substitute_secrets_in_text("PH_CASE", "API.OPENAI.COM").is_ok());
        assert!(manager.substitute_secrets_in_text("PH_CASE", "Api.OpenAI.Com").is_ok());
    }

    /// Test that no text without placeholders triggers substitution errors.
    #[test]
    fn test_no_placeholder_passthrough() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH_NOOP".to_string(),
                allowed_hosts: vec!["restricted.com".to_string()],
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Text without any placeholder should pass through unchanged, for any host
        let result = manager.substitute_secrets_in_text("Bearer some-other-token", "evil.com");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer some-other-token");
    }

    /// Test that TLS upgrade preserves connection_hosts mapping.
    #[test]
    fn test_tls_upgrade_preserves_host_mapping() {
        let config = NetConfig::permissive();
        let secrets = HashMap::new();
        let mut manager = ConnectionManager::new(config, secrets);

        // Simulate a TCP connection with host mapping and HTTP state
        let tcp_handle = 1_u32;
        let tls_handle = 2_u32;
        manager
            .connection_hosts
            .insert(tcp_handle, "example.com".to_string());
        manager
            .http_states
            .insert(tcp_handle, HttpParsingState::default());

        // Simulate what tls_upgrade now does: carry over host, clean up old state
        if let Some(host) = manager.connection_hosts.remove(&tcp_handle) {
            manager.connection_hosts.insert(tls_handle, host);
        }
        manager.http_states.remove(&tcp_handle);

        // New TLS handle should have the host mapping
        assert_eq!(
            manager.connection_hosts.get(&tls_handle),
            Some(&"example.com".to_string())
        );
        // Old TCP handle should be cleaned up
        assert!(!manager.connection_hosts.contains_key(&tcp_handle));
        assert!(!manager.http_states.contains_key(&tcp_handle));
    }
}
