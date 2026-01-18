//! TLS networking support for the sandbox.
//!
//! This module provides secure network connections for Python code running
//! in the sandbox. All connections use TLS - no plaintext TCP is exposed.
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
    /// Connection timeout for TCP + TLS handshake.
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
        }
    }
}

/// Errors that can occur during TLS operations.
#[derive(Debug, Clone)]
pub enum TlsError {
    /// Connection was refused by the remote host.
    ConnectionRefused,
    /// Connection was reset by the remote host.
    ConnectionReset,
    /// Operation timed out.
    TimedOut,
    /// DNS lookup failed.
    HostNotFound,
    /// TLS handshake or certificate verification failed.
    TlsHandshakeFailed(String),
    /// Generic I/O error.
    IoError(String),
    /// Network access not permitted by sandbox policy.
    NotPermitted(String),
    /// Invalid handle (connection was closed).
    InvalidHandle,
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionRefused => write!(f, "connection refused"),
            Self::ConnectionReset => write!(f, "connection reset"),
            Self::TimedOut => write!(f, "timed out"),
            Self::HostNotFound => write!(f, "host not found"),
            Self::TlsHandshakeFailed(msg) => write!(f, "TLS handshake failed: {msg}"),
            Self::IoError(msg) => write!(f, "I/O error: {msg}"),
            Self::NotPermitted(msg) => write!(f, "not permitted: {msg}"),
            Self::InvalidHandle => write!(f, "invalid handle"),
        }
    }
}

impl std::error::Error for TlsError {}

/// Manages TLS connections for a sandbox instance.
///
/// Each sandbox has its own connection manager, which tracks active connections
/// and enforces the network policy.
#[derive(Debug)]
pub struct TlsConnectionManager {
    config: NetConfig,
    tls_config: Arc<ClientConfig>,
    connections: HashMap<u32, TlsStream<TcpStream>>,
    next_handle: u32,
}

impl TlsConnectionManager {
    /// Create a new connection manager with the given config.
    #[must_use]
    pub fn new(config: NetConfig) -> Self {
        // Build rustls config with system root certs
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        Self {
            config,
            tls_config: Arc::new(tls_config),
            connections: HashMap::new(),
            next_handle: 1,
        }
    }

    /// Connect to a host with TLS.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The host is blocked by policy
    /// - DNS lookup fails
    /// - TCP connection fails
    /// - TLS handshake fails
    /// - Connection limit is reached
    pub async fn connect(&mut self, host: &str, port: u16) -> Result<u32, TlsError> {
        // 1. Check host against allowed/blocked patterns
        self.check_host_allowed(host)?;

        // 2. Check connection limit
        if self.config.max_connections > 0
            && self.connections.len() >= self.config.max_connections as usize
        {
            return Err(TlsError::NotPermitted("connection limit reached".into()));
        }

        // 3. DNS resolve + TCP connect (with timeout)
        let addr = tokio::time::timeout(
            self.config.connect_timeout,
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| TlsError::TimedOut)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TlsError::HostNotFound
            } else {
                TlsError::IoError(e.to_string())
            }
        })?
        .next()
        .ok_or(TlsError::HostNotFound)?;

        let tcp = tokio::time::timeout(self.config.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| TlsError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused => TlsError::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset => TlsError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TlsError::TimedOut,
                _ => TlsError::IoError(e.to_string()),
            })?;

        // 4. TLS handshake (with timeout)
        let connector = TlsConnector::from(self.tls_config.clone());
        let server_name: ServerName<'static> = host
            .to_string()
            .try_into()
            .map_err(|_| TlsError::TlsHandshakeFailed("invalid hostname".into()))?;

        let tls = tokio::time::timeout(
            self.config.connect_timeout,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| TlsError::TimedOut)?
        .map_err(|e| TlsError::TlsHandshakeFailed(e.to_string()))?;

        // 5. Store and return handle
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        if self.next_handle == 0 {
            self.next_handle = 1; // Skip 0 to avoid confusion with "no handle"
        }
        self.connections.insert(handle, tls);

        tracing::debug!(handle, host, port, "TLS connection established");
        Ok(handle)
    }

    /// Read up to `len` bytes from a connection.
    ///
    /// Returns the bytes read. May return fewer than `len` bytes.
    /// Returns an empty vec on EOF.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle is invalid or I/O fails.
    pub async fn read(&mut self, handle: u32, len: u32) -> Result<Vec<u8>, TlsError> {
        let stream = self
            .connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let mut buf = vec![0u8; len as usize];
        let n = tokio::time::timeout(self.config.io_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| TlsError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TlsError::TimedOut,
                _ => TlsError::IoError(e.to_string()),
            })?;

        buf.truncate(n);
        Ok(buf)
    }

    /// Write bytes to a connection.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle is invalid or I/O fails.
    pub async fn write(&mut self, handle: u32, data: &[u8]) -> Result<u32, TlsError> {
        let stream = self
            .connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let n = tokio::time::timeout(self.config.io_timeout, stream.write(data))
            .await
            .map_err(|_| TlsError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TlsError::TimedOut,
                _ => TlsError::IoError(e.to_string()),
            })?;

        Ok(n as u32)
    }

    /// Close a connection.
    ///
    /// After this call, the handle is invalid.
    pub fn close(&mut self, handle: u32) {
        if self.connections.remove(&handle).is_some() {
            tracing::debug!(handle, "TLS connection closed");
        }
        // TLS shutdown happens on drop
    }

    /// Check if a host is allowed by the current policy.
    fn check_host_allowed(&self, host: &str) -> Result<(), TlsError> {
        // Check blocked first
        for pattern in &self.config.blocked_hosts {
            if host_matches_pattern(host, pattern) {
                return Err(TlsError::NotPermitted(format!("host '{host}' is blocked")));
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
                return Err(TlsError::NotPermitted(format!(
                    "host '{host}' not in allowed list"
                )));
            }
        }

        Ok(())
    }
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
    if let Some(last) = parts.last() {
        if !last.is_empty() && pos != host.len() {
            return false;
        }
    }

    true
}

#[cfg(test)]
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
        let manager = TlsConnectionManager::new(config);

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
        let manager = TlsConnectionManager::new(config);

        assert!(manager.check_host_allowed("api.example.com").is_ok());
        assert!(manager.check_host_allowed("api.github.com").is_ok());
        assert!(manager.check_host_allowed("google.com").is_err());
    }

    #[test]
    fn test_permissive_config() {
        let config = NetConfig::permissive();
        let manager = TlsConnectionManager::new(config);

        assert!(manager.check_host_allowed("localhost").is_ok());
        assert!(manager.check_host_allowed("127.0.0.1").is_ok());
        assert!(manager.check_host_allowed("google.com").is_ok());
    }
}
