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
/// create a `NetConfig` and pass it to the Sandbox. A default `NetConfig()`
/// allows no hosts: list them in `allowed_hosts` (or with `allow_host()`),
/// or set `allow_all_hosts=True` to allow every non-blocked host.
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
    /// Assigning it turns allow_all_hosts off (see `set_allowed_hosts`).
    #[pyo3(get)]
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

    /// Substitute secret placeholders on plain TCP connections too, not only
    /// after a TLS upgrade. Off by default: turning it on sends real secret
    /// values unencrypted over `http://`.
    #[pyo3(get, set)]
    pub allow_plaintext_secrets: bool,

    /// Method and path rules for HTTP requests (see `http_rules`).
    http_rules: Vec<eryx::HttpRule>,

    /// Custom root certificates (DER-encoded bytes).
    custom_root_certs: Vec<Vec<u8>>,
}

/// HTTP requests allowed to the hosts matching a pattern.
///
/// A host matching any rule's pattern only accepts requests that match one
/// of its rules; other hosts are unaffected. Once any rule is set, every
/// request is parsed and checked against the connected host and the hosts
/// it names (`Host` header or absolute URL), requests with ambiguous framing
/// are refused, and only HTTP/1.1 may be sent to a host with rules. A
/// refused request is never sent, and the write raises `OSError` in the
/// sandbox.
///
/// Methods are compared ignoring case. Paths match whole segments:
/// `/repos/org/repo` matches it and anything under `/repos/org/repo/`, but
/// not `/repos/org/repo-evil`; a path ending in `/` matches anything under
/// it. Paths are matched without the query and with percent-escapes
/// decoded; a path with `.`/`..` or empty segments, or an escaped `/` or
/// `\`, never matches. Queries are regular expressions (Rust `regex`
/// syntax), each matched against the whole raw query string: the text after
/// `?` as sent, not decoded, and empty when there's none.
/// `service=git-upload-pack` matches only that query, and
/// `(service=git-upload-pack)?` also no query.
///
/// Example:
///     net = NetConfig(
///         allowed_hosts=["api.github.com"],
///         http_rules=[
///             HttpRule("api.github.com", methods=["GET", "POST"], paths=["/repos/org/repo"]),
///         ],
///     )
#[pyclass(module = "eryx", frozen, from_py_object)]
#[derive(Debug, Clone)]
pub struct HttpRule(pub(crate) eryx::HttpRule);

#[pymethods]
impl HttpRule {
    /// Create a rule for hosts matching `host` (same patterns as
    /// `allowed_hosts`).
    ///
    /// Args:
    ///     host: Host pattern the rule applies to.
    ///     methods: Allowed methods; None or empty allows any.
    ///     paths: Allowed path prefixes; None or empty allows any. Must be
    ///         normalized absolute paths (no `.`/`..` or empty segments, query
    ///         or escapes).
    ///     queries: Allowed queries, as regular expressions matched against
    ///         the whole raw query; None or empty allows any.
    ///
    /// Raises:
    ///     ValueError: If a path can't match any request, or a query
    ///         pattern isn't a valid regular expression.
    #[new]
    #[pyo3(signature = (host, *, methods=None, paths=None, queries=None))]
    fn new(
        host: String,
        methods: Option<Vec<String>>,
        paths: Option<Vec<String>>,
        queries: Option<Vec<String>>,
    ) -> PyResult<Self> {
        let rule = eryx::HttpRule::new(host)
            .with_methods(methods.unwrap_or_default())
            .with_paths(paths.unwrap_or_default())
            .with_queries(queries.unwrap_or_default());
        eryx::NetConfig::default()
            .allow_http(rule.clone())
            .validate()
            .map_err(PyValueError::new_err)?;
        Ok(Self(rule))
    }

    /// The host pattern.
    #[getter]
    fn host(&self) -> &str {
        self.0.host()
    }

    /// The allowed methods; empty allows any.
    #[getter]
    fn methods(&self) -> Vec<String> {
        self.0.methods().to_vec()
    }

    /// The allowed path prefixes; empty allows any.
    #[getter]
    fn paths(&self) -> Vec<String> {
        self.0.paths().to_vec()
    }

    /// The allowed query patterns; empty allows any.
    #[getter]
    fn queries(&self) -> Vec<String> {
        self.0.queries().to_vec()
    }

    fn __repr__(&self) -> String {
        format!(
            "HttpRule({:?}, methods={:?}, paths={:?}, queries={:?})",
            self.0.host(),
            self.0.methods(),
            self.0.paths(),
            self.0.queries()
        )
    }
}

#[pymethods]
impl NetConfig {
    /// Create new network configuration.
    ///
    /// By default:
    /// - max_connections: 10
    /// - connect_timeout_ms: 30000 (30 seconds)
    /// - io_timeout_ms: 60000 (60 seconds)
    /// - allow_all_hosts: false (only allowed_hosts may be reached)
    /// - allowed_hosts: [] (no hosts allowed)
    /// - blocked_hosts: localhost and private networks
    /// - blocked_networks: every non-public range (loopback, private,
    ///   link-local/cloud metadata, CGNAT, multicast, ...)
    /// - allow_plaintext_secrets: false (secrets substituted only over TLS)
    ///
    /// Args:
    ///     max_connections: Maximum number of concurrent connections.
    ///     connect_timeout_ms: Timeout for establishing connections. Must be greater than zero.
    ///     io_timeout_ms: Timeout for read/write operations. Must be greater than zero.
    ///     allow_all_hosts: When true, all non-blocked hosts are allowed (allowed_hosts is ignored).
    ///     allowed_hosts: List of allowed host patterns (supports wildcards like "*.example.com").
    ///     blocked_hosts: List of blocked host patterns.
    ///     blocked_networks: CIDRs (e.g. "10.0.0.0/8") that a host's resolved
    ///         addresses must not fall in. Checked after DNS, so a public name
    ///         pointing at an internal address is refused.
    ///     allowed_networks: CIDRs exempt from blocked_networks.
    ///     internal_hosts: Host patterns allowed to resolve into blocked_networks.
    ///         Only list names whose DNS you control.
    ///     allow_plaintext_secrets: Also substitute secret placeholders on plain
    ///         TCP (e.g. `http://`), not only after a TLS upgrade. This sends the
    ///         real secret value unencrypted, where anyone on the network path
    ///         can read it; only enable it for trusted local test servers.
    ///     http_rules: `HttpRule`s limiting the methods and paths requests to
    ///         their hosts may use.
    ///
    /// Raises:
    ///     ValueError: If a network is not a valid CIDR.
    ///
    /// Example:
    ///     # Only allow specific APIs
    ///     net = NetConfig(allowed_hosts=["api.example.com", "*.openai.com"])
    ///
    ///     # Any public host, with the default blocks
    ///     net = NetConfig(allow_all_hosts=True)
    #[new]
    #[pyo3(signature = (*, max_connections=10, connect_timeout_ms=30000, io_timeout_ms=60000, allow_all_hosts=false, allowed_hosts=None, blocked_hosts=None, blocked_networks=None, allowed_networks=None, internal_hosts=None, allow_plaintext_secrets=false, http_rules=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        max_connections: u32,
        connect_timeout_ms: u64,
        io_timeout_ms: u64,
        allow_all_hosts: bool,
        allowed_hosts: Option<Vec<String>>,
        blocked_hosts: Option<Vec<String>>,
        blocked_networks: Option<Vec<String>>,
        allowed_networks: Option<Vec<String>>,
        internal_hosts: Option<Vec<String>>,
        allow_plaintext_secrets: bool,
        http_rules: Option<Vec<HttpRule>>,
    ) -> PyResult<Self> {
        Ok(Self {
            max_connections,
            connect_timeout_ms,
            io_timeout_ms,
            allow_all_hosts,
            allowed_hosts: allowed_hosts.unwrap_or_default(),
            blocked_hosts: blocked_hosts.unwrap_or_else(default_blocked_hosts),
            blocked_networks: match blocked_networks {
                Some(cidrs) => parse_networks(&cidrs)?,
                None => eryx::DEFAULT_BLOCKED_NETWORKS.to_vec(),
            },
            allowed_networks: parse_networks(&allowed_networks.unwrap_or_default())?,
            internal_hosts: internal_hosts.unwrap_or_default(),
            allow_plaintext_secrets,
            http_rules: http_rules.into_iter().flatten().map(|r| r.0).collect(),
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
            allow_plaintext_secrets: false,
            http_rules: vec![],
            custom_root_certs: vec![],
        }
    }

    #[setter]
    fn set_allowed_hosts(&mut self, patterns: Vec<String>) {
        self.allowed_hosts = patterns;
        self.allow_all_hosts = false;
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

    /// Method and path rules for HTTP requests; see `HttpRule`.
    #[getter]
    fn http_rules(&self) -> Vec<HttpRule> {
        self.http_rules.iter().cloned().map(HttpRule).collect()
    }

    #[setter]
    fn set_http_rules(&mut self, rules: Vec<HttpRule>) {
        self.http_rules = rules.into_iter().map(|r| r.0).collect();
    }

    /// Add an `HttpRule`.
    ///
    /// Returns self for method chaining.
    ///
    /// Example:
    ///     net = NetConfig(allowed_hosts=["api.github.com"]).allow_http(
    ///         HttpRule("api.github.com", methods=["GET"], paths=["/repos/org/repo"])
    ///     )
    fn allow_http(&mut self, rule: HttpRule) -> Self {
        self.http_rules.push(rule.0);
        self.clone()
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
    ///     net = NetConfig(allowed_hosts=["svc.cluster.local"]).allow_network("10.96.0.0/12")
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
    ///     net = NetConfig(allowed_hosts=["api.internal.example.com"]).allow_internal_host(
    ///         "api.internal.example.com"
    ///     )
    fn allow_internal_host(&mut self, pattern: String) -> Self {
        self.internal_hosts.push(pattern);
        self.clone()
    }

    /// Add a host pattern to the allowed list.
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`.
    /// Turns allow_all_hosts off, so only the allowed hosts can be reached.
    ///
    /// Returns self for method chaining.
    ///
    /// Example:
    ///     net = NetConfig().allow_host("api.example.com").allow_host("*.openai.com")
    fn allow_host(&mut self, pattern: String) -> Self {
        self.allowed_hosts.push(pattern);
        self.allow_all_hosts = false;
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
    ///     net = NetConfig(allowed_hosts=["localhost"]).allow_localhost()
    fn allow_localhost(&mut self) -> Self {
        self.blocked_hosts
            .retain(|p| !p.contains("localhost") && !p.starts_with("127.") && !p.contains("::1"));
        self.blocked_networks.retain(|n| !n.addr().is_loopback());
        // `::1` is also inside the default `::/96` block.
        let loopback = IpNet::from(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
        if !self.allowed_networks.contains(&loopback) {
            self.allowed_networks.push(loopback);
        }
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
            "NetConfig(max_connections={}, connect_timeout_ms={}, io_timeout_ms={}, allow_all_hosts={}, allowed_hosts={:?}, blocked_hosts=[{} patterns], blocked_networks=[{} networks], allowed_networks={:?}, internal_hosts={:?}, allow_plaintext_secrets={}, http_rules=[{} rules])",
            self.max_connections,
            self.connect_timeout_ms,
            self.io_timeout_ms,
            self.allow_all_hosts,
            self.allowed_hosts,
            self.blocked_hosts.len(),
            self.blocked_networks.len(),
            self.allowed_networks(),
            self.internal_hosts,
            self.allow_plaintext_secrets,
            self.http_rules.len(),
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
        let net = Self::default()
            .with_max_connections(config.max_connections)
            .with_connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .with_io_timeout(Duration::from_millis(config.io_timeout_ms))
            .allow_all_hosts(config.allow_all_hosts)
            .with_allowed_hosts(config.allowed_hosts.clone())
            .with_blocked_hosts(config.blocked_hosts.clone())
            .with_blocked_networks(config.blocked_networks.clone())
            .with_allowed_networks(config.allowed_networks.clone())
            .with_internal_hosts(config.internal_hosts.clone())
            .with_http_rules(config.http_rules.clone())
            .allow_plaintext_secrets(config.allow_plaintext_secrets);
        config
            .custom_root_certs
            .iter()
            .cloned()
            .fold(net, Self::with_root_cert)
    }
}

impl From<NetConfig> for eryx::NetConfig {
    fn from(config: NetConfig) -> Self {
        (&config).into()
    }
}
