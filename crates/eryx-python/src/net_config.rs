//! NetConfig wrapper for Python.
//!
//! Exposes sandbox network configuration to Python.

use eryx::IpNet;
use pyo3::exceptions::PyValueError;
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
#[pyclass(module = "eryx", from_py_object)]
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

    /// When true, all non-blocked hosts are allowed (allowed_hosts is ignored).
    #[pyo3(get, set)]
    pub allow_all_hosts: bool,

    /// Allowed host patterns (only checked when allow_all_hosts is false).
    #[pyo3(get, set)]
    pub allowed_hosts: Vec<String>,

    /// Blocked host patterns.
    #[pyo3(get, set)]
    pub blocked_hosts: Vec<String>,

    /// CIDRs that resolved addresses must not fall in (see `blocked_networks`).
    blocked_networks: Vec<IpNet>,

    /// CIDRs exempt from `blocked_networks` (see `allowed_networks`).
    allowed_networks: Vec<IpNet>,

    /// Host patterns allowed to resolve into `blocked_networks`, for trusted
    /// names routed internally on purpose (e.g. via cluster DNS).
    #[pyo3(get, set)]
    pub internal_hosts: Vec<String>,

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
    /// - allow_all_hosts: true (all external hosts allowed)
    /// - allowed_hosts: [] (only used when allow_all_hosts is false)
    /// - blocked_hosts: localhost and private networks
    /// - blocked_networks: every non-public range (loopback, private,
    ///   link-local/cloud metadata, CGNAT, multicast, ...)
    ///
    /// Args:
    ///     max_connections: Maximum number of concurrent connections.
    ///     connect_timeout_ms: Timeout for establishing connections.
    ///     io_timeout_ms: Timeout for read/write operations.
    ///     allow_all_hosts: When true, all non-blocked hosts are allowed (allowed_hosts is ignored).
    ///     allowed_hosts: List of allowed host patterns (supports wildcards like "*.example.com").
    ///         Setting this implicitly sets allow_all_hosts to false.
    ///     blocked_hosts: List of blocked host patterns.
    ///     blocked_networks: CIDRs (e.g. "10.0.0.0/8") that a host's resolved
    ///         addresses must not fall in. Checked after DNS, so a public name
    ///         pointing at an internal address is refused.
    ///     allowed_networks: CIDRs exempt from blocked_networks.
    ///     internal_hosts: Host patterns allowed to resolve into blocked_networks.
    ///         Only list names whose DNS you control.
    ///
    /// Raises:
    ///     ValueError: If a network is not a valid CIDR.
    ///
    /// Example:
    ///     # Only allow specific APIs
    ///     net = NetConfig(allowed_hosts=["api.example.com", "*.openai.com"])
    ///
    ///     # Custom timeouts
    ///     net = NetConfig(connect_timeout_ms=5000, io_timeout_ms=10000)
    #[new]
    #[pyo3(signature = (*, max_connections=10, connect_timeout_ms=30000, io_timeout_ms=60000, allow_all_hosts=None, allowed_hosts=None, blocked_hosts=None, blocked_networks=None, allowed_networks=None, internal_hosts=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        max_connections: u32,
        connect_timeout_ms: u64,
        io_timeout_ms: u64,
        allow_all_hosts: Option<bool>,
        allowed_hosts: Option<Vec<String>>,
        blocked_hosts: Option<Vec<String>>,
        blocked_networks: Option<Vec<String>>,
        allowed_networks: Option<Vec<String>>,
        internal_hosts: Option<Vec<String>>,
    ) -> PyResult<Self> {
        // If allowed_hosts is provided and allow_all_hosts wasn't explicitly set,
        // default to false (allowlist mode).
        let allow_all =
            allow_all_hosts.unwrap_or_else(|| allowed_hosts.as_ref().is_none_or(|h| h.is_empty()));
        Ok(Self {
            max_connections,
            connect_timeout_ms,
            io_timeout_ms,
            allow_all_hosts: allow_all,
            allowed_hosts: allowed_hosts.unwrap_or_default(),
            blocked_hosts: blocked_hosts.unwrap_or_else(default_blocked_hosts),
            blocked_networks: match blocked_networks {
                Some(cidrs) => parse_networks(&cidrs)?,
                None => eryx::DEFAULT_BLOCKED_NETWORKS.to_vec(),
            },
            allowed_networks: parse_networks(&allowed_networks.unwrap_or_default())?,
            internal_hosts: internal_hosts.unwrap_or_default(),
            custom_root_certs: vec![],
        })
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
            allow_all_hosts: true,
            allowed_hosts: vec![],
            blocked_hosts: vec![],
            blocked_networks: vec![],
            allowed_networks: vec![],
            internal_hosts: vec![],
            custom_root_certs: vec![],
        }
    }

    /// CIDRs that a host's resolved addresses must not fall in.
    #[getter]
    fn blocked_networks(&self) -> Vec<String> {
        self.blocked_networks
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[setter]
    fn set_blocked_networks(&mut self, cidrs: Vec<String>) -> PyResult<()> {
        self.blocked_networks = parse_networks(&cidrs)?;
        Ok(())
    }

    /// CIDRs exempt from `blocked_networks`.
    #[getter]
    fn allowed_networks(&self) -> Vec<String> {
        self.allowed_networks
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[setter]
    fn set_allowed_networks(&mut self, cidrs: Vec<String>) -> PyResult<()> {
        self.allowed_networks = parse_networks(&cidrs)?;
        Ok(())
    }

    /// Allow addresses in a CIDR even though they fall in `blocked_networks`.
    ///
    /// Returns self for method chaining.
    ///
    /// Raises:
    ///     ValueError: If `cidr` is not a valid CIDR.
    ///
    /// Example:
    ///     net = NetConfig().allow_network("10.96.0.0/12")
    fn allow_network(&mut self, cidr: String) -> PyResult<Self> {
        self.allowed_networks.extend(parse_networks(&[cidr])?);
        Ok(self.clone())
    }

    /// Let hosts matching a pattern resolve to internal addresses.
    ///
    /// For trusted names routed internally on purpose (e.g. a service reached
    /// through cluster DNS). Only list names whose DNS you control.
    ///
    /// Returns self for method chaining.
    ///
    /// Example:
    ///     net = NetConfig().allow_internal_host("api.internal.example.com")
    fn allow_internal_host(&mut self, pattern: String) -> Self {
        self.internal_hosts.push(pattern);
        self.clone()
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
        self.blocked_networks.retain(|n| !n.addr().is_loopback());
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
            "NetConfig(max_connections={}, connect_timeout_ms={}, io_timeout_ms={}, allow_all_hosts={}, allowed_hosts={:?}, blocked_hosts=[{} patterns])",
            self.max_connections,
            self.connect_timeout_ms,
            self.io_timeout_ms,
            self.allow_all_hosts,
            self.allowed_hosts,
            self.blocked_hosts.len(),
        )
    }
}

/// Parse CIDR strings, raising `ValueError` on the first invalid one.
fn parse_networks(cidrs: &[String]) -> PyResult<Vec<IpNet>> {
    cidrs
        .iter()
        .map(|c| {
            c.parse()
                .map_err(|e| PyValueError::new_err(format!("invalid CIDR '{c}': {e}")))
        })
        .collect()
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
            allow_all_hosts: config.allow_all_hosts,
            allowed_hosts: config.allowed_hosts.clone(),
            blocked_hosts: config.blocked_hosts.clone(),
            blocked_networks: config.blocked_networks.clone(),
            allowed_networks: config.allowed_networks.clone(),
            internal_hosts: config.internal_hosts.clone(),
            resolver: None,
            custom_root_certs: config.custom_root_certs.clone(),
        }
    }
}

impl From<NetConfig> for eryx::NetConfig {
    fn from(config: NetConfig) -> Self {
        (&config).into()
    }
}
