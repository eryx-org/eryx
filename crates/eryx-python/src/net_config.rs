//! NetConfig wrapper for Python.
//!
//! Exposes sandbox network configuration to Python.

use pyo3::prelude::*;
use std::time::Duration;

/// Network configuration for sandbox execution.
///
/// Use this class to configure which hosts Python code can connect to,
/// set timeouts, and add custom certificates.
///
/// By default, network access is **disabled**. To enable networking,
/// create a `NetConfig` and pass it to the Sandbox.
///
/// Example:
///     # Allow connections to external APIs only
///     net = NetConfig(
///         allowed_hosts=["api.example.com", "*.googleapis.com"],
///     )
///     sandbox = Sandbox(network=net)
///
///     # Allow localhost for local development
///     net = NetConfig.permissive()
///     sandbox = Sandbox(network=net)
#[pyclass(module = "eryx")]
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Maximum concurrent connections.
    #[pyo3(get, set)]
    pub max_connections: u32,

    /// Connection timeout in milliseconds.
    #[pyo3(get, set)]
    pub connect_timeout_ms: u64,

    /// I/O timeout in milliseconds.
    #[pyo3(get, set)]
    pub io_timeout_ms: u64,

    /// Allowed host patterns (empty = allow all external hosts).
    #[pyo3(get, set)]
    pub allowed_hosts: Vec<String>,

    /// Blocked host patterns.
    #[pyo3(get, set)]
    pub blocked_hosts: Vec<String>,

    /// Custom root certificates (DER-encoded bytes).
    custom_root_certs: Vec<Vec<u8>>,
}

#[pymethods]
impl NetConfig {
    /// Create new network configuration.
    ///
    /// By default:
    /// - max_connections: 10
    /// - connect_timeout_ms: 30000 (30 seconds)
    /// - io_timeout_ms: 60000 (60 seconds)
    /// - allowed_hosts: [] (allow all external hosts)
    /// - blocked_hosts: localhost and private networks
    ///
    /// Args:
    ///     max_connections: Maximum number of concurrent connections.
    ///     connect_timeout_ms: Timeout for establishing connections.
    ///     io_timeout_ms: Timeout for read/write operations.
    ///     allowed_hosts: List of allowed host patterns (supports wildcards like "*.example.com").
    ///     blocked_hosts: List of blocked host patterns.
    ///
    /// Example:
    ///     # Only allow specific APIs
    ///     net = NetConfig(allowed_hosts=["api.example.com", "*.openai.com"])
    ///
    ///     # Custom timeouts
    ///     net = NetConfig(connect_timeout_ms=5000, io_timeout_ms=10000)
    #[new]
    #[pyo3(signature = (*, max_connections=10, connect_timeout_ms=30000, io_timeout_ms=60000, allowed_hosts=None, blocked_hosts=None))]
    fn new(
        max_connections: u32,
        connect_timeout_ms: u64,
        io_timeout_ms: u64,
        allowed_hosts: Option<Vec<String>>,
        blocked_hosts: Option<Vec<String>>,
    ) -> Self {
        Self {
            max_connections,
            connect_timeout_ms,
            io_timeout_ms,
            allowed_hosts: allowed_hosts.unwrap_or_default(),
            blocked_hosts: blocked_hosts.unwrap_or_else(default_blocked_hosts),
            custom_root_certs: vec![],
        }
    }

    /// Create a permissive config that allows all hosts including localhost.
    ///
    /// Warning: This allows sandbox code to access local services. Use with caution.
    ///
    /// Example:
    ///     net = NetConfig.permissive()
    ///     sandbox = Sandbox(network=net)
    #[staticmethod]
    fn permissive() -> Self {
        Self {
            max_connections: 100,
            connect_timeout_ms: 30000,
            io_timeout_ms: 60000,
            allowed_hosts: vec![],
            blocked_hosts: vec![],
            custom_root_certs: vec![],
        }
    }

    /// Add a host pattern to the allowed list.
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`
    ///
    /// Returns self for method chaining.
    ///
    /// Example:
    ///     net = NetConfig().allow_host("api.example.com").allow_host("*.openai.com")
    fn allow_host(&mut self, pattern: String) -> Self {
        self.allowed_hosts.push(pattern);
        self.clone()
    }

    /// Add a host pattern to the blocked list.
    ///
    /// Returns self for method chaining.
    fn block_host(&mut self, pattern: String) -> Self {
        self.blocked_hosts.push(pattern);
        self.clone()
    }

    /// Allow connections to localhost (disabled by default).
    ///
    /// Returns self for method chaining.
    ///
    /// Example:
    ///     net = NetConfig().allow_localhost()
    fn allow_localhost(&mut self) -> Self {
        self.blocked_hosts
            .retain(|p| !p.contains("localhost") && !p.starts_with("127.") && !p.contains("::1"));
        self.clone()
    }

    /// Add a custom root certificate (DER-encoded bytes).
    ///
    /// This is useful for testing with self-signed certificates.
    ///
    /// Args:
    ///     cert_der: The certificate in DER format as bytes.
    ///
    /// Returns self for method chaining.
    fn with_root_cert(&mut self, cert_der: Vec<u8>) -> Self {
        self.custom_root_certs.push(cert_der);
        self.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "NetConfig(max_connections={}, connect_timeout_ms={}, io_timeout_ms={}, allowed_hosts={:?}, blocked_hosts=[{} patterns])",
            self.max_connections,
            self.connect_timeout_ms,
            self.io_timeout_ms,
            self.allowed_hosts,
            self.blocked_hosts.len(),
        )
    }
}

/// Default blocked hosts (localhost and private networks).
fn default_blocked_hosts() -> Vec<String> {
    vec![
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
    ]
}

impl From<&NetConfig> for eryx::NetConfig {
    fn from(config: &NetConfig) -> Self {
        Self {
            max_connections: config.max_connections,
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            io_timeout: Duration::from_millis(config.io_timeout_ms),
            allowed_hosts: config.allowed_hosts.clone(),
            blocked_hosts: config.blocked_hosts.clone(),
            custom_root_certs: config.custom_root_certs.clone(),
        }
    }
}

impl From<NetConfig> for eryx::NetConfig {
    fn from(config: NetConfig) -> Self {
        (&config).into()
    }
}
