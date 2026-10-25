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
//! - Private/local networks are blocked by default, checked against the
//!   *resolved* address so a public name pointing at an internal IP is refused
//! - Connection limits prevent resource exhaustion

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

pub use ipnet::IpNet;
use ipnet::{Ipv4Net, Ipv6Net};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Largest buffer a single socket read will allocate, regardless of the length
/// the guest asked for (1 MiB).
///
/// The read length is guest-controlled, and the buffer is allocated on the
/// *host*, outside the sandbox's memory limit. Socket reads are short reads by
/// nature (`recv(n)` may always return fewer than `n` bytes), so a guest that
/// wants more simply reads again.
const MAX_READ_LEN: usize = 1024 * 1024;

/// Buffer size to allocate for a guest-requested read of `len` bytes.
///
/// Split out from the read paths so the bound itself is testable: the failure
/// mode it prevents (a multi-gigabyte host allocation) is invisible end-to-end,
/// because sockets return short reads either way.
fn read_buf_len(len: u32) -> usize {
    (len as usize).min(MAX_READ_LEN)
}

/// Resolves hostnames to socket addresses for sandbox connections.
///
/// Set one with [`NetConfig::with_resolver`] to replace the system resolver
/// (e.g. to use specific nameservers, or pin names to addresses). The network
/// policy still applies to whatever it returns: a resolver decides where a
/// name points, not whether the sandbox may connect there.
///
/// # Example
///
/// ```
/// use std::net::SocketAddr;
///
/// use eryx::{NetConfig, Resolver};
///
/// /// Pins one name to a fixed address; everything else uses the system resolver.
/// #[derive(Debug)]
/// struct Pinned(SocketAddr);
///
/// #[async_trait::async_trait]
/// impl Resolver for Pinned {
///     async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
///         if host == "api.example.com" {
///             return Ok(vec![SocketAddr::new(self.0.ip(), port)]);
///         }
///         Ok(tokio::net::lookup_host((host, port)).await?.collect())
///     }
/// }
///
/// let config = NetConfig::default()
///     .allow_host("api.example.com")
///     .with_resolver(Pinned("93.184.215.14:0".parse().unwrap()));
/// # let _ = config;
/// ```
#[async_trait::async_trait]
pub trait Resolver: Send + Sync + std::fmt::Debug {
    /// Resolve `host` to the addresses to try, in order of preference.
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
}

/// Network configuration for the sandbox.
///
/// Controls which hosts Python code can connect to and sets timeouts.
///
/// A connection is checked twice: the requested host against
/// `blocked_hosts`/`allowed_hosts`, then every address it resolves to against
/// `blocked_networks`/`allowed_networks`. Only an address that passes is
/// connected to, so a DNS answer can't swap in an internal one.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Maximum concurrent connections (0 = unlimited).
    pub max_connections: u32,
    /// Connection timeout for TCP connect.
    pub connect_timeout: Duration,
    /// Read/write timeout for I/O operations.
    pub io_timeout: Duration,
    /// When true, all non-blocked hosts are allowed (`allowed_hosts` is ignored).
    /// When false (default), only hosts matching `allowed_hosts` are permitted.
    pub allow_all_hosts: bool,
    /// Allowed host patterns (only checked when `allow_all_hosts` is false).
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`, `exact.host.com`
    pub allowed_hosts: Vec<String>,
    /// Blocked host patterns (checked after allowed).
    ///
    /// By default, blocks localhost and private networks.
    pub blocked_hosts: Vec<String>,
    /// Networks that resolved addresses must not fall in.
    ///
    /// By default, every non-public range: see [`DEFAULT_BLOCKED_NETWORKS`].
    /// IPv4-mapped IPv6 addresses are checked as IPv4.
    pub blocked_networks: Vec<IpNet>,
    /// Exceptions to `blocked_networks`: an address in one of these is allowed
    /// even if it also falls in a blocked network.
    pub allowed_networks: Vec<IpNet>,
    /// Host patterns allowed to resolve into `blocked_networks`.
    ///
    /// For trusted names that are routed internally on purpose (e.g. a
    /// service reached through cluster DNS). The host must still pass
    /// `blocked_hosts`/`allowed_hosts`. Only list names whose DNS you control:
    /// whoever answers for them decides which internal address they reach.
    pub internal_hosts: Vec<String>,
    /// Resolver for hostnames; `None` uses the system resolver.
    pub resolver: Option<Arc<dyn Resolver>>,
    /// Custom root certificates (DER-encoded) to trust in addition to system certs.
    ///
    /// Useful for testing with self-signed certificates.
    pub custom_root_certs: Vec<Vec<u8>>,
    /// Substitute secret placeholders on plain TCP connections too.
    ///
    /// By default (`false`) placeholders are only replaced after a TLS
    /// upgrade, so a real secret never crosses the network unencrypted. A
    /// request over plain TCP (e.g. `http://`) is sent with the placeholder
    /// left in place.
    pub allow_plaintext_secrets: bool,
}

/// The default [`NetConfig::blocked_networks`]: every range that isn't
/// publicly routable unicast.
///
/// Loopback, private (RFC 1918 and IPv6 unique-local), link-local (which
/// includes cloud metadata endpoints such as `169.254.169.254`), carrier-grade
/// NAT, "this network", IETF protocol assignments, benchmarking, multicast,
/// reserved/broadcast, local-use NAT64 and 6to4. Well-known NAT64 addresses
/// (`64:ff9b::/96`) are checked by the IPv4 address they embed.
pub const DEFAULT_BLOCKED_NETWORKS: [IpNet; 18] = {
    const fn v4(a: u8, b: u8, c: u8, d: u8, len: u8) -> IpNet {
        IpNet::V4(Ipv4Net::new_assert(Ipv4Addr::new(a, b, c, d), len))
    }
    const fn v6(first: u16, last: u16, len: u8) -> IpNet {
        IpNet::V6(Ipv6Net::new_assert(
            Ipv6Addr::new(first, 0, 0, 0, 0, 0, 0, last),
            len,
        ))
    }
    [
        v4(0, 0, 0, 0, 8),
        v4(10, 0, 0, 0, 8),
        v4(100, 64, 0, 0, 10),
        v4(127, 0, 0, 0, 8),
        v4(169, 254, 0, 0, 16),
        v4(172, 16, 0, 0, 12),
        v4(192, 0, 0, 0, 24),
        v4(192, 168, 0, 0, 16),
        v4(198, 18, 0, 0, 15),
        v4(224, 0, 0, 0, 4),
        v4(240, 0, 0, 0, 4),
        v6(0, 0, 128),
        v6(0, 1, 128),
        v6(0x2002, 0, 16),
        IpNet::V6(Ipv6Net::new_assert(
            Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0),
            48,
        )),
        v6(0xfc00, 0, 7),
        v6(0xfe80, 0, 10),
        v6(0xff00, 0, 8),
    ]
};

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            connect_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(60),
            allow_all_hosts: false,
            allowed_hosts: vec![],
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
            blocked_networks: DEFAULT_BLOCKED_NETWORKS.to_vec(),
            allowed_networks: vec![],
            internal_hosts: vec![],
            resolver: None,
            custom_root_certs: vec![],
            allow_plaintext_secrets: false,
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

    /// Allow addresses in `network` even though they fall in `blocked_networks`.
    #[must_use]
    pub fn allow_network(mut self, network: IpNet) -> Self {
        self.allowed_networks.push(network);
        self
    }

    /// Add a network to `blocked_networks`.
    #[must_use]
    pub fn block_network(mut self, network: IpNet) -> Self {
        self.blocked_networks.push(network);
        self
    }

    /// Let hosts matching `pattern` resolve to internal addresses; see
    /// [`NetConfig::internal_hosts`].
    #[must_use]
    pub fn allow_internal_host(mut self, pattern: impl Into<String>) -> Self {
        self.internal_hosts.push(pattern.into());
        self
    }

    /// Resolve hostnames with `resolver` instead of the system resolver.
    #[must_use]
    pub fn with_resolver(mut self, resolver: impl Resolver + 'static) -> Self {
        self.resolver = Some(Arc::new(resolver));
        self
    }

    /// Allow connections to localhost (disabled by default).
    #[must_use]
    pub fn allow_localhost(mut self) -> Self {
        self.blocked_hosts
            .retain(|p| !p.contains("localhost") && !p.starts_with("127.") && !p.contains("::1"));
        self.blocked_networks.retain(|n| !n.addr().is_loopback());
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
            allow_all_hosts: true,
            allowed_hosts: vec![],
            blocked_hosts: vec![],
            blocked_networks: vec![],
            allowed_networks: vec![],
            internal_hosts: vec![],
            resolver: None,
            custom_root_certs: vec![],
            allow_plaintext_secrets: false,
        }
    }

    /// Substitute secret placeholders on plain TCP connections as well as TLS;
    /// see [`NetConfig::allow_plaintext_secrets`].
    #[must_use]
    pub fn allow_plaintext_secrets(mut self, allow: bool) -> Self {
        self.allow_plaintext_secrets = allow;
        self
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
#[non_exhaustive]
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
#[non_exhaustive]
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

/// HTTP request framing state for a single connection.
#[derive(Debug, Default)]
struct HttpParsingState {
    /// Request bytes buffered until the headers are complete.
    buffer: Vec<u8>,
    /// How much of the current request's body is still to come.
    body: Body,
}

/// The body of the request currently being written.
#[derive(Debug, Default)]
enum Body {
    /// No request in progress: the next bytes start a request's headers.
    #[default]
    None,
    /// A `Content-Length` body with this many bytes left.
    Length(usize),
    /// A chunked body, at this point in its framing.
    Chunked(Chunked),
}

/// Position within a `Transfer-Encoding: chunked` body.
#[derive(Debug)]
enum Chunked {
    /// On a chunk-size line; `ext` once past the hex digits.
    Size { size: usize, ext: bool },
    /// Inside chunk data, with this many bytes left.
    Data(usize),
    /// On the line break after chunk data.
    DataEnd,
    /// In the trailer section; `line` is the current line's length so far.
    Trailer { line: usize },
}

impl Chunked {
    const START: Self = Self::Size {
        size: 0,
        ext: false,
    };

    /// Advance over `data`, returning the length of the prefix that ends the
    /// body if it ends within `data`.
    fn advance(&mut self, data: &[u8]) -> Result<Option<usize>, TcpError> {
        let malformed = || TcpError::IoError("malformed chunked request body".into());
        let mut i = 0;
        while i < data.len() {
            let byte = data[i];
            match self {
                Self::Data(left) => {
                    let n = (*left).min(data.len() - i);
                    *left -= n;
                    i += n;
                    if *left == 0 {
                        *self = Self::DataEnd;
                    }
                    continue;
                }
                Self::Size { size, ext } => match byte {
                    b'\n' if *size == 0 => *self = Self::Trailer { line: 0 },
                    b'\n' => *self = Self::Data(*size),
                    b'\r' => {}
                    _ if *ext => {}
                    b';' | b' ' | b'\t' => *ext = true,
                    _ => {
                        let digit = char::from(byte).to_digit(16).ok_or_else(malformed)?;
                        *size = size
                            .checked_mul(16)
                            .and_then(|s| s.checked_add(digit as usize))
                            .ok_or_else(malformed)?;
                    }
                },
                Self::DataEnd => {
                    if byte == b'\n' {
                        *self = Self::START;
                    }
                }
                Self::Trailer { line } => match byte {
                    b'\n' if *line == 0 => return Ok(Some(i + 1)),
                    b'\n' => *line = 0,
                    b'\r' => {}
                    _ => *line += 1,
                },
            }
            i += 1;
        }
        Ok(None)
    }
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
            if let Err(e) = root_store.add(cert) {
                tracing::warn!(error = %e, "ignoring invalid custom root certificate");
            }
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

    /// Resolve the effective timeout for an operation.
    ///
    /// `guest_ms` is the timeout the sandboxed code requested (e.g. via
    /// `socket.settimeout`), in milliseconds, where `0` means "no preference,
    /// use the host default". The result is always clamped to the host's
    /// configured `default_timeout`, so a guest can shorten a timeout but never
    /// extend it beyond the sandbox's configured ceiling.
    fn effective_timeout(default_timeout: Duration, guest_ms: u32) -> Duration {
        if guest_ms == 0 {
            default_timeout
        } else {
            default_timeout.min(Duration::from_millis(u64::from(guest_ms)))
        }
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

    /// Run guest-written bytes through HTTP request framing, substituting
    /// secret placeholders in each request's headers.
    ///
    /// This is the shared logic used by both `tcp_write()` and `tls_write()`.
    /// Returns the bytes to send now: empty while a request's headers are
    /// still incomplete.
    fn process_write_data(&mut self, handle: u32, data: &[u8]) -> Result<Vec<u8>, TcpError> {
        let mut state = self.http_states.remove(&handle).unwrap_or_default();
        let result = self.frame_requests(handle, &mut state, data);
        self.http_states.insert(handle, state);
        result
    }

    /// Advance `state` over `data`, one request at a time, so every request
    /// on a keep-alive connection gets its headers processed.
    fn frame_requests(
        &self,
        handle: u32,
        state: &mut HttpParsingState,
        data: &[u8],
    ) -> Result<Vec<u8>, TcpError> {
        let mut out = Vec::new();
        let mut input = data.to_vec();
        let mut pos = 0;
        loop {
            match &mut state.body {
                Body::None => {
                    state.buffer.extend_from_slice(&input[pos..]);
                    let Some(header_end) = find_header_end(&state.buffer) else {
                        break;
                    };
                    input = state.buffer.split_off(header_end);
                    pos = 0;
                    let headers = std::mem::take(&mut state.buffer);

                    // Not HTTP - pass through everything
                    if !is_http_request(&headers) {
                        out.extend_from_slice(&headers);
                        out.extend_from_slice(&input);
                        break;
                    }

                    // Check for HTTP/2 (not supported with secrets)
                    if is_http2(&headers) {
                        return Err(TcpError::NotPermitted(
                            "HTTP/2 with secrets not supported. Use HTTP/1.1.".into(),
                        ));
                    }

                    let substituted = self.substitute_http_headers(&headers, handle)?;
                    // Chunked framing takes precedence over Content-Length.
                    state.body = if is_chunked(&substituted) {
                        Body::Chunked(Chunked::START)
                    } else {
                        match extract_content_length(&substituted) {
                            Some(len) if len > 0 => Body::Length(len),
                            _ => Body::None,
                        }
                    };
                    out.extend_from_slice(&substituted);
                }
                Body::Length(left) => {
                    if pos == input.len() {
                        break;
                    }
                    let n = (*left).min(input.len() - pos);
                    out.extend_from_slice(&input[pos..pos + n]);
                    pos += n;
                    *left -= n;
                    if *left == 0 {
                        state.body = Body::None;
                    }
                }
                Body::Chunked(chunked) => {
                    if pos == input.len() {
                        break;
                    }
                    let rest = &input[pos..];
                    let n = match chunked.advance(rest)? {
                        Some(n) => {
                            state.body = Body::None;
                            n
                        }
                        None => rest.len(),
                    };
                    out.extend_from_slice(&rest[..n]);
                    pos += n;
                }
            }
        }
        Ok(out)
    }

    /// Check if a host is allowed by the current policy.
    fn check_host_allowed(&self, host: &str) -> Result<(), TcpError> {
        // Check blocked first (always enforced).
        for pattern in &self.config.blocked_hosts {
            if host_matches_pattern(host, pattern) {
                return Err(TcpError::NotPermitted(format!("host '{host}' is blocked")));
            }
        }

        // If allow_all_hosts is set, skip the allowlist check.
        if self.config.allow_all_hosts {
            return Ok(());
        }

        // Otherwise host must match the allowlist.
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

        Ok(())
    }

    /// Check a resolved address against `blocked_networks`/`allowed_networks`.
    ///
    /// An address matches a network in either its own form or the IPv4
    /// address it stands for, so `::ffff:10.0.0.0/104` and `10.0.0.0/8` both
    /// match `::ffff:10.0.0.1`.
    fn is_ip_allowed(&self, ip: IpAddr) -> bool {
        let embedded = embedded_ipv4(ip);
        let matches = |nets: &[IpNet]| {
            nets.iter()
                .any(|n| n.contains(&ip) || embedded.is_some_and(|v4| n.contains(&v4)))
        };
        !matches(&self.config.blocked_networks) || matches(&self.config.allowed_networks)
    }

    /// Resolve `host` and keep the addresses the policy allows.
    ///
    /// Callers must connect to one of the returned addresses, never re-resolve:
    /// the check is only as good as the address actually used.
    async fn resolve_allowed(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, TcpError> {
        let addrs: Vec<SocketAddr> = match &self.config.resolver {
            Some(resolver) => resolver.resolve(host, port).await,
            None => tokio::net::lookup_host((host, port))
                .await
                .map(Iterator::collect),
        }
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TcpError::HostNotFound
            } else {
                TcpError::IoError(e.to_string())
            }
        })?;
        if addrs.is_empty() {
            return Err(TcpError::HostNotFound);
        }

        let internal = self
            .config
            .internal_hosts
            .iter()
            .any(|p| host_matches_pattern(host, p));
        if internal {
            return Ok(addrs);
        }

        let (allowed, blocked): (Vec<_>, Vec<_>) =
            addrs.into_iter().partition(|a| self.is_ip_allowed(a.ip()));
        if allowed.is_empty() {
            // The address stays in the host's logs: telling the guest would let
            // it map internal DNS names to addresses.
            tracing::debug!(host, addr = %blocked[0].ip(), "host resolves to a blocked network");
            return Err(TcpError::NotPermitted(format!(
                "host '{host}' resolves to a blocked network"
            )));
        }
        Ok(allowed)
    }

    // ========================================================================
    // TCP operations
    // ========================================================================

    /// Connect to a host over TCP.
    ///
    /// `timeout_ms` is the guest-requested connect timeout in milliseconds
    /// (`0` = use the host default); see `effective_timeout`.
    pub async fn tcp_connect(
        &mut self,
        host: &str,
        port: u16,
        timeout_ms: u32,
    ) -> Result<u32, TcpError> {
        // 1. Check host against allowed/blocked patterns
        self.check_host_allowed(host)?;

        // 2. Check connection limit
        if self.config.max_connections > 0
            && self.connection_count() >= self.config.max_connections as usize
        {
            return Err(TcpError::NotPermitted("connection limit reached".into()));
        }

        let connect_timeout = Self::effective_timeout(self.config.connect_timeout, timeout_ms);

        // 3. DNS resolve + check resolved addresses + TCP connect (with timeout)
        let addrs = tokio::time::timeout(connect_timeout, self.resolve_allowed(host, port))
            .await
            .map_err(|_| TcpError::TimedOut)??;

        // Tries each checked address in turn, never re-resolving.
        let tcp = tokio::time::timeout(connect_timeout, TcpStream::connect(&addrs[..]))
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
    ///
    /// `timeout_ms` is the guest-requested I/O timeout in milliseconds
    /// (`0` = use the host default); see `effective_timeout`.
    ///
    /// `len` is capped at 1 MiB; like any socket read, this may return fewer
    /// bytes than requested.
    pub async fn tcp_read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, TcpError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        let mut buf = vec![0u8; read_buf_len(len)];
        let n = tokio::time::timeout(io_timeout, stream.read(&mut buf))
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
    /// If secrets are configured and [`NetConfig::allow_plaintext_secrets`] is
    /// set, this method will parse HTTP requests and substitute secret
    /// placeholders in headers before sending. Otherwise bytes are sent as
    /// written, placeholders included.
    pub async fn tcp_write(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<u32, TcpError> {
        if !self.tcp_connections.contains_key(&handle) {
            return Err(TcpError::InvalidHandle);
        }

        // Fast path: nothing to substitute = no parsing
        if self.secrets.is_empty() || !self.config.allow_plaintext_secrets {
            self.tcp_write_raw(handle, data, timeout_ms).await?;
        } else {
            let bytes = self.process_write_data(handle, data)?;
            if !bytes.is_empty() {
                self.tcp_write_raw(handle, &bytes, timeout_ms).await?;
            }
        }
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write all of `data` to a TCP connection without HTTP parsing.
    async fn tcp_write_raw(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<(), TcpError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        tokio::time::timeout(io_timeout, stream.write_all(data))
            .await
            .map_err(|_| TcpError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })
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
            // The blank line ending the headers is re-added below.
            if line.is_empty() {
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
    ///
    /// # Host Authorization Logic
    ///
    /// 1. If `secret.allowed_hosts` is non-empty, use that list
    /// 2. Otherwise, fall back to `NetConfig.allowed_hosts`
    /// 3. If both are empty, the secret is allowed for ANY host
    ///    (⚠️ this may be surprising - always specify allowed_hosts for production)
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
            // Falls back to NetConfig.allowed_hosts if secret has no explicit restrictions
            let allowed_hosts = if secret_config.allowed_hosts.is_empty() {
                &self.config.allowed_hosts
            } else {
                &secret_config.allowed_hosts
            };

            // ⚠️ If both secret and NetConfig have empty allowed_hosts,
            // the secret can go to ANY host. This is intentional for flexibility
            // but users should always specify allowed_hosts for production.
            let host_allowed = if allowed_hosts.is_empty() {
                true
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
    pub async fn tls_upgrade(
        &mut self,
        tcp_handle: u32,
        hostname: &str,
        timeout_ms: u32,
    ) -> Result<u32, TlsError> {
        let connect_timeout = Self::effective_timeout(self.config.connect_timeout, timeout_ms);

        // Remove the TCP connection (we're taking ownership). Its handle is
        // gone whether or not the handshake succeeds, so drop its state now.
        let tcp = self
            .tcp_connections
            .remove(&tcp_handle)
            .ok_or(TlsError::InvalidHandle)?;
        let host = self.connection_hosts.remove(&tcp_handle);
        self.http_states.remove(&tcp_handle);

        // TLS handshake
        let connector = TlsConnector::from(self.tls_config.clone());
        let server_name: ServerName<'static> = hostname
            .to_string()
            .try_into()
            .map_err(|_| TlsError::HandshakeFailed("invalid hostname".into()))?;

        let tls = tokio::time::timeout(connect_timeout, connector.connect(server_name, tcp))
            .await
            .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
            .map_err(|e| TlsError::HandshakeFailed(e.to_string()))?;

        let handle = self.alloc_handle();
        self.tls_connections.insert(handle, tls);

        // Carry over the host mapping from the TCP connection
        if let Some(host) = host {
            self.connection_hosts.insert(handle, host);
        }

        tracing::debug!(
            handle,
            hostname,
            "TLS connection established (upgraded from TCP handle {})",
            tcp_handle
        );
        Ok(handle)
    }

    /// Read from a TLS connection.
    ///
    /// `timeout_ms` is the guest-requested I/O timeout in milliseconds
    /// (`0` = use the host default); see `effective_timeout`.
    ///
    /// `len` is capped at 1 MiB; like any socket read, this may return fewer
    /// bytes than requested.
    pub async fn tls_read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, TlsError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let mut buf = vec![0u8; read_buf_len(len)];
        let n = tokio::time::timeout(io_timeout, stream.read(&mut buf))
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
    pub async fn tls_write(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<u32, TlsError> {
        if !self.tls_connections.contains_key(&handle) {
            return Err(TlsError::InvalidHandle);
        }

        // Fast path: no secrets = no parsing
        if self.secrets.is_empty() {
            self.tls_write_raw(handle, data, timeout_ms).await?;
        } else {
            let bytes = self.process_write_data(handle, data)?;
            if !bytes.is_empty() {
                self.tls_write_raw(handle, &bytes, timeout_ms).await?;
            }
        }
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write all of `data` to a TLS connection without HTTP parsing.
    ///
    /// Flushes too, so no TLS records stay buffered once the call returns.
    async fn tls_write_raw(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<(), TlsError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let write = async {
            stream.write_all(data).await?;
            stream.flush().await
        };
        tokio::time::timeout(io_timeout, write)
            .await
            .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            })
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

    /// Close all active connections, clearing per-connection state.
    ///
    /// Drops every TCP and TLS stream (triggering OS-level close / TLS
    /// shutdown) and removes all HTTP parsing and host-tracking state.
    ///
    /// Preserved across the call:
    /// - `config` and `tls_config` (network policy)
    /// - `secrets` (secret substitution rules)
    /// - `next_handle` (stays monotonic — stale guest handles remain invalid)
    pub fn close_all_connections(&mut self) {
        let tcp_count = self.tcp_connections.len();
        let tls_count = self.tls_connections.len();
        self.tcp_connections.clear();
        self.tls_connections.clear();
        self.http_states.clear();
        self.connection_hosts.clear();
        tracing::debug!(tcp_count, tls_count, "all connections closed");
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

/// The IPv4 address an IPv6 address stands for on the wire, if any:
/// IPv4-mapped (`::ffff:a.b.c.d`) or well-known-prefix NAT64
/// (`64:ff9b::a.b.c.d`, RFC 6052), which a NAT64 gateway translates to that
/// IPv4 destination.
fn embedded_ipv4(ip: IpAddr) -> Option<IpAddr> {
    let IpAddr::V6(v6) = ip else { return None };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4.into());
    }
    let [a, b, rest @ ..] = v6.segments();
    if (a, b) == (0x64, 0xff9b) && rest[..4] == [0, 0, 0, 0] {
        let [.., w, x, y, z] = v6.octets();
        return Some(Ipv4Addr::new(w, x, y, z).into());
    }
    None
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

    // Glob matching with * as wildcard: the first part anchors at the
    // start, the last at the end, and the parts between match left to right.
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if host.len() < first.len() + last.len() || !host.starts_with(first) || !host.ends_with(last) {
        return false;
    }

    let mut rest = &host[first.len()..host.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(idx) => rest = &rest[idx + part.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// `len` is guest-controlled and the buffer lands on the host, outside the
    /// sandbox's memory limit: `recv(2**32 - 1)` used to allocate 4 GiB per call.
    #[test]
    fn read_buf_len_is_bounded() {
        use super::{MAX_READ_LEN, read_buf_len};

        // Small reads are honoured exactly - no forced round-up.
        assert_eq!(read_buf_len(0), 0);
        assert_eq!(read_buf_len(1), 1);
        assert_eq!(read_buf_len(4096), 4096);

        // At and past the cap, the buffer stops growing.
        assert_eq!(read_buf_len(MAX_READ_LEN as u32), MAX_READ_LEN);
        assert_eq!(read_buf_len(MAX_READ_LEN as u32 + 1), MAX_READ_LEN);
        assert_eq!(read_buf_len(u32::MAX), MAX_READ_LEN);
    }

    use super::*;

    #[test]
    fn test_effective_timeout_uses_default_when_guest_zero() {
        // A guest timeout of 0 means "no preference" - fall back to the host default.
        let default = Duration::from_secs(30);
        assert_eq!(
            ConnectionManager::effective_timeout(default, 0),
            default,
            "guest_ms == 0 should use the host default"
        );
    }

    #[test]
    fn test_effective_timeout_guest_can_shorten() {
        // A guest timeout below the host ceiling shortens the effective timeout.
        let default = Duration::from_secs(30);
        assert_eq!(
            ConnectionManager::effective_timeout(default, 1000),
            Duration::from_millis(1000),
            "a shorter guest timeout should win"
        );
    }

    #[test]
    fn test_effective_timeout_guest_cannot_exceed_host() {
        // The host ceiling caps the guest: a script must not be able to extend
        // a timeout beyond the sandbox's configured limit.
        let default = Duration::from_secs(5);
        assert_eq!(
            ConnectionManager::effective_timeout(default, 60_000),
            default,
            "a longer guest timeout must be clamped to the host ceiling"
        );
    }

    #[test]
    fn test_effective_timeout_equal_is_stable() {
        let default = Duration::from_secs(2);
        assert_eq!(
            ConnectionManager::effective_timeout(default, 2000),
            default,
            "an equal guest timeout should match the host ceiling exactly"
        );
    }

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

        // The trailing literal anchors at the end, even when it also occurs earlier
        assert!(host_matches_pattern(
            "a.example.com.example.com",
            "*.example.com"
        ));
        assert!(host_matches_pattern("api.com.foo.com", "api.*.com"));
        assert!(!host_matches_pattern("api.com", "api.*.com"));
    }

    #[test]
    fn test_default_config_blocks_everything() {
        let config = NetConfig::default();
        let manager = ConnectionManager::new(config, HashMap::new());

        // Default is allow_all_hosts=false with empty allowed_hosts, so nothing is permitted.
        assert!(manager.check_host_allowed("localhost").is_err());
        assert!(manager.check_host_allowed("127.0.0.1").is_err());
        assert!(manager.check_host_allowed("192.168.1.1").is_err());
        assert!(manager.check_host_allowed("10.0.0.1").is_err());
        assert!(manager.check_host_allowed("google.com").is_err());
        assert!(manager.check_host_allowed("api.example.com").is_err());
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
    fn test_allow_all_hosts_true() {
        let config = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new());

        // allow_all_hosts=true permits all non-blocked hosts.
        assert!(manager.check_host_allowed("google.com").is_ok());
        assert!(manager.check_host_allowed("api.example.com").is_ok());
        // Blocked hosts are still blocked.
        assert!(manager.check_host_allowed("localhost").is_err());
        assert!(manager.check_host_allowed("127.0.0.1").is_err());
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
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "evil.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "127.0.0.1")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "localhost")
                .is_err()
        );
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
        assert!(
            manager
                .substitute_secrets_in_text("PH_OPENAI", "api.openai.com")
                .is_ok()
        );
        // OpenAI key for GitHub host: REJECTED
        assert!(
            manager
                .substitute_secrets_in_text("PH_OPENAI", "api.github.com")
                .is_err()
        );
        // GitHub key for GitHub host: OK
        assert!(
            manager
                .substitute_secrets_in_text("PH_GITHUB", "api.github.com")
                .is_ok()
        );
        // GitHub key for OpenAI host: REJECTED
        assert!(
            manager
                .substitute_secrets_in_text("PH_GITHUB", "api.openai.com")
                .is_err()
        );
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

        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "api.openai.com")
                .is_ok()
        );
        // Bare domain doesn't match *.openai.com
        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "evil.com")
                .is_err()
        );
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
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "api.openai.com.evil.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "api-openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "evil-api.openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "xapi.openai.com")
                .is_err()
        );
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
        assert!(
            manager
                .substitute_secrets_in_text("PH_IP", "104.18.7.192")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_IP", "1.2.3.4")
                .is_err()
        );
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
        assert!(
            manager
                .substitute_secrets_in_text("PH_CASE", "API.OPENAI.COM")
                .is_ok()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_CASE", "Api.OpenAI.Com")
                .is_ok()
        );
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

    /// Test that very large HTTP headers are handled correctly without security bypass.
    /// Ensures that secrets in large headers are still properly substituted.
    #[test]
    fn test_large_http_headers_with_secrets() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real-secret-value".to_string(),
                placeholder: "PLACEHOLDER_LARGE".to_string(),
                allowed_hosts: vec!["api.example.com".to_string()],
            },
        );

        let mut manager = ConnectionManager::new(config, secrets);

        // Simulate a connection to allowed host
        let handle = 1_u32;
        manager
            .connection_hosts
            .insert(handle, "api.example.com".to_string());

        // Create a large header block with many headers and a secret at the end
        let mut large_headers = String::from("GET /api HTTP/1.1\r\n");
        large_headers.push_str("Host: api.example.com\r\n");

        // Add many large headers (simulate a request with lots of metadata)
        for i in 0..100 {
            let padding = "X".repeat(500); // 500 char value
            large_headers.push_str(&format!("X-Header-{i}: {padding}\r\n"));
        }

        // Add the secret header at the end
        large_headers.push_str("Authorization: Bearer PLACEHOLDER_LARGE\r\n");
        large_headers.push_str("\r\n");

        // Verify header block is indeed large (> 50KB)
        assert!(
            large_headers.len() > 50_000,
            "Test headers should be > 50KB, got {} bytes",
            large_headers.len()
        );

        // Substitute headers
        let result = manager.substitute_http_headers(large_headers.as_bytes(), handle);
        assert!(result.is_ok(), "Large header substitution should succeed");

        let substituted = String::from_utf8(result.unwrap()).unwrap();

        // Verify the secret was substituted
        assert!(
            substituted.contains("Authorization: Bearer real-secret-value"),
            "Secret should be substituted in large headers"
        );
        assert!(
            !substituted.contains("PLACEHOLDER_LARGE"),
            "Placeholder should not remain in substituted headers"
        );
    }

    /// Test that TLS SNI hostname does NOT affect secret authorization.
    /// The host check must use the TCP connection target, not the TLS SNI.
    /// This prevents an attack where an attacker connects to evil.com but
    /// sets TLS SNI to api.openai.com to try to trick secret substitution.
    #[test]
    fn test_tls_sni_does_not_override_host_check() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real-secret-value".to_string(),
                placeholder: "PLACEHOLDER_SNI".to_string(),
                allowed_hosts: vec!["api.openai.com".to_string()],
            },
        );

        let mut manager = ConnectionManager::new(config, secrets);

        // Simulate: TCP connected to evil.com
        let tcp_handle = 1_u32;
        manager
            .connection_hosts
            .insert(tcp_handle, "evil.com".to_string());

        // Simulate: TLS upgrade with SNI="api.openai.com" (the allowed host)
        // In real code, tls_upgrade would copy the host from tcp to tls handle.
        // The key security property: connection_hosts stores the TCP target, not SNI.
        let tls_handle = 2_u32;
        // The host should remain "evil.com" from the original TCP connection
        if let Some(host) = manager.connection_hosts.get(&tcp_handle) {
            manager.connection_hosts.insert(tls_handle, host.clone());
        }

        // Now try to substitute secrets - should fail because connection_hosts has "evil.com"
        let headers = b"GET /api HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer PLACEHOLDER_SNI\r\n\r\n";
        let result = manager.substitute_http_headers(headers, tls_handle);

        assert!(
            result.is_err(),
            "Secret substitution must use TCP target (evil.com), not TLS SNI or Host header (api.openai.com)"
        );
    }

    /// Test that secrets in large headers are blocked for unauthorized hosts.
    #[test]
    fn test_large_headers_unauthorized_host() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real-secret-value".to_string(),
                placeholder: "PLACEHOLDER_BLOCKED".to_string(),
                allowed_hosts: vec!["api.legitimate.com".to_string()],
            },
        );

        let mut manager = ConnectionManager::new(config, secrets);

        // Simulate a connection to UNAUTHORIZED host
        let handle = 1_u32;
        manager
            .connection_hosts
            .insert(handle, "evil.com".to_string());

        // Create large headers with the secret
        let mut large_headers = String::from("POST /steal HTTP/1.1\r\n");
        large_headers.push_str("Host: evil.com\r\n");

        // Add padding
        for i in 0..50 {
            let padding = "Y".repeat(1000);
            large_headers.push_str(&format!("X-Pad-{i}: {padding}\r\n"));
        }

        // Secret header buried in the middle
        large_headers.push_str("X-Stolen: PLACEHOLDER_BLOCKED\r\n");
        large_headers.push_str("\r\n");

        // Should fail because host is not authorized for this secret
        let result = manager.substitute_http_headers(large_headers.as_bytes(), handle);
        assert!(
            result.is_err(),
            "Secret substitution should fail for unauthorized host even with large headers"
        );
    }

    #[tokio::test]
    async fn close_all_connections_clears_connections_and_state() {
        let config = NetConfig::permissive();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PH".to_string(),
                allowed_hosts: vec![],
            },
        );

        let mut manager = ConnectionManager::new(config, secrets);

        // Allocate handles and insert fake per-connection state.
        let h1 = manager.alloc_handle();
        let h2 = manager.alloc_handle();
        manager
            .connection_hosts
            .insert(h1, "a.example.com".to_string());
        manager
            .connection_hosts
            .insert(h2, "b.example.com".to_string());
        manager.http_states.insert(h1, HttpParsingState::default());
        manager.http_states.insert(h2, HttpParsingState::default());

        // Create a real TCP pair so tcp_connections has entries.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, _server) = tokio::join!(tokio::net::TcpStream::connect(addr), async {
            listener.accept().await.unwrap().0
        },);
        manager.tcp_connections.insert(h1, client.unwrap());

        let next_before = manager.next_handle;
        assert_eq!(manager.connection_count(), 1);
        assert_eq!(manager.connection_hosts.len(), 2);
        assert_eq!(manager.http_states.len(), 2);

        manager.close_all_connections();

        // Connections and per-connection state are cleared.
        assert_eq!(manager.connection_count(), 0);
        assert!(manager.connection_hosts.is_empty());
        assert!(manager.http_states.is_empty());

        // Config, secrets, and handle counter are preserved.
        assert_eq!(manager.next_handle, next_before);
        assert!(manager.secrets.contains_key("KEY"));
        assert!(manager.config.allow_all_hosts);
    }

    #[tokio::test]
    async fn stale_handles_are_invalid_after_close_all() {
        let config = NetConfig::permissive();
        let mut manager = ConnectionManager::new(config, HashMap::new());

        // Create a real connection and store it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, _server) = tokio::join!(tokio::net::TcpStream::connect(addr), async {
            listener.accept().await.unwrap().0
        },);
        let handle = manager.alloc_handle();
        manager.tcp_connections.insert(handle, client.unwrap());
        assert_eq!(manager.connection_count(), 1);

        manager.close_all_connections();

        // The stale handle now produces an InvalidHandle error.
        let result = manager.tcp_read(handle, 1, 0).await;
        assert!(
            matches!(result, Err(TcpError::InvalidHandle)),
            "read on stale handle after close_all should return InvalidHandle, got {result:?}"
        );

        let result = manager.tcp_write(handle, b"x", 0).await;
        assert!(
            matches!(result, Err(TcpError::InvalidHandle)),
            "write on stale handle after close_all should return InvalidHandle, got {result:?}"
        );
    }

    #[test]
    fn close_all_on_empty_manager_is_noop() {
        let mut manager = ConnectionManager::new(NetConfig::permissive(), HashMap::new());
        let next_before = manager.next_handle;

        manager.close_all_connections();

        assert_eq!(manager.connection_count(), 0);
        assert_eq!(manager.next_handle, next_before);
    }

    #[test]
    fn handle_counter_advances_after_close_all() {
        let mut manager = ConnectionManager::new(NetConfig::permissive(), HashMap::new());
        let h1 = manager.alloc_handle();
        let h2 = manager.alloc_handle();

        manager.close_all_connections();

        let h3 = manager.alloc_handle();
        assert!(
            h3 > h2,
            "handle after close_all ({h3}) should be greater than handle before ({h2})"
        );
        assert_ne!(h3, h1);
        assert_ne!(h3, h2);
    }

    /// Answers every lookup with fixed addresses, standing in for a hostile or
    /// split-horizon DNS server.
    #[derive(Debug)]
    struct FixedResolver(Vec<IpAddr>);

    #[async_trait::async_trait]
    impl Resolver for FixedResolver {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn test_default_blocked_networks() {
        let manager = ConnectionManager::new(NetConfig::default(), HashMap::new());
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::",
            "::1",
            "fd00:ec2::254",
            "fe80::1",
            // IPv4-mapped IPv6 is the IPv4 address on the wire.
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            // So is a well-known NAT64 address, through the gateway.
            "64:ff9b::a00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:a00:1::1",
            "198.18.0.1",
        ] {
            assert!(!manager.is_ip_allowed(ip(blocked)), "{blocked} allowed");
        }
        for public in [
            "8.8.8.8",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "64:ff9b::808:808",
        ] {
            assert!(manager.is_ip_allowed(ip(public)), "{public} blocked");
        }
    }

    #[test]
    fn test_allowed_networks_override_blocked() {
        let config = NetConfig::default().allow_network("10.96.0.0/12".parse().unwrap());
        let manager = ConnectionManager::new(config, HashMap::new());
        assert!(manager.is_ip_allowed(ip("10.96.12.34")));
        assert!(manager.is_ip_allowed(ip("::ffff:10.96.12.34")));
        assert!(!manager.is_ip_allowed(ip("10.0.0.1")));
    }

    /// A configured IPv4-mapped CIDR still matches, though addresses are also
    /// checked as IPv4.
    #[test]
    fn test_mapped_ipv6_networks_match() {
        let config = NetConfig {
            blocked_networks: vec!["::ffff:8.8.8.0/120".parse().unwrap()],
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new());
        assert!(!manager.is_ip_allowed(ip("::ffff:8.8.8.8")));
        assert!(manager.is_ip_allowed(ip("8.8.4.4")));
    }

    #[test]
    fn test_allow_localhost_unblocks_loopback_networks_only() {
        let manager =
            ConnectionManager::new(NetConfig::default().allow_localhost(), HashMap::new());
        assert!(manager.is_ip_allowed(ip("127.0.0.1")));
        assert!(manager.is_ip_allowed(ip("::1")));
        assert!(!manager.is_ip_allowed(ip("10.0.0.1")));
        assert!(!manager.is_ip_allowed(ip("169.254.169.254")));
    }

    /// The hostname passes every pattern check, but its DNS answer is internal.
    #[tokio::test]
    async fn test_public_name_resolving_to_internal_ip_is_blocked() {
        let config = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default()
        }
        .with_resolver(FixedResolver(vec![ip("169.254.169.254")]));
        let mut manager = ConnectionManager::new(config, HashMap::new());

        let err = manager
            .tcp_connect("public.example", 80, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, TcpError::NotPermitted(m) if m.contains("resolves to a blocked network") && !m.contains("169.254")),
            "{err}"
        );
    }

    /// A mixed answer is narrowed to the allowed addresses, so the connection
    /// can't land on the internal one.
    #[tokio::test]
    async fn test_mixed_dns_answer_keeps_only_allowed_addresses() {
        let config =
            NetConfig::default().with_resolver(FixedResolver(vec![ip("127.0.0.1"), ip("8.8.8.8")]));
        let manager = ConnectionManager::new(config, HashMap::new());

        let addrs = manager
            .resolve_allowed("public.example", 443)
            .await
            .unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(ip("8.8.8.8"), 443)]);
    }

    /// `internal_hosts` lets a trusted name reach an internal address, which
    /// is how a service routed through cluster DNS stays reachable.
    #[tokio::test]
    async fn test_internal_hosts_may_resolve_to_blocked_networks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default()
        }
        .allow_internal_host("assistant.example.com")
        .with_resolver(FixedResolver(vec![ip("127.0.0.1")]));
        let mut manager = ConnectionManager::new(config, HashMap::new());

        manager
            .tcp_connect("assistant.example.com", port, 0)
            .await
            .unwrap();
        // Other names resolving to the same address are still refused.
        assert!(matches!(
            manager.tcp_connect("other.example.com", port, 0).await,
            Err(TcpError::NotPermitted(_))
        ));
    }

    /// A manager with one secret (`PH` -> `real`) and a fake connection to
    /// `api.example.com` on handle 1, for driving `process_write_data`.
    fn framing_manager() -> ConnectionManager {
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            crate::secrets::SecretConfig {
                real_value: "real".to_string(),
                placeholder: "PH".to_string(),
                allowed_hosts: vec![],
            },
        );
        let mut manager = ConnectionManager::new(
            NetConfig::permissive().allow_plaintext_secrets(true),
            secrets,
        );
        manager
            .connection_hosts
            .insert(1, "api.example.com".to_string());
        manager
    }

    /// Feed each write through the framing and concatenate what goes out.
    fn frame(manager: &mut ConnectionManager, writes: &[&[u8]]) -> String {
        let mut out = Vec::new();
        for write in writes {
            out.extend(manager.process_write_data(1, write).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    const GET: &[u8] = b"GET / HTTP/1.1\r\nAuthorization: PH\r\n\r\n";
    const GET_OUT: &str = "GET / HTTP/1.1\r\nAuthorization: real\r\n\r\n";

    /// Every request on a keep-alive connection gets its headers processed,
    /// whatever framing the request before it used.
    #[test]
    fn test_keep_alive_requests_all_substituted() {
        let mut m = framing_manager();
        assert_eq!(frame(&mut m, &[GET, GET]), format!("{GET_OUT}{GET_OUT}"));

        let mut m = framing_manager();
        let post = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n";
        assert_eq!(
            frame(&mut m, &[post, b"he", b"llo", GET]),
            format!("POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello{GET_OUT}")
        );

        let mut m = framing_manager();
        let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let body = "5;ext=1\r\nPH PH\r\na\r\n0123456789\r\n0\r\nX-Trailer: PH\r\n\r\n";
        // Split the body at every byte to exercise each framing state.
        let pieces: Vec<&[u8]> = body.as_bytes().chunks(1).collect();
        let mut writes = vec![&chunked[..]];
        writes.extend(pieces);
        writes.push(GET);
        assert_eq!(
            frame(&mut m, &writes),
            format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{body}{GET_OUT}")
        );
    }

    /// Pipelined requests in a single write are framed one by one.
    #[test]
    fn test_pipelined_requests_in_one_write() {
        let mut m = framing_manager();
        let mut write = b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi".to_vec();
        write.extend_from_slice(GET);
        write.extend_from_slice(b"GET /next");
        assert_eq!(
            frame(&mut m, &[&write]),
            format!("POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi{GET_OUT}")
        );
        // The partial third request waits for the rest of its headers.
        assert_eq!(
            frame(&mut m, &[b" HTTP/1.1\r\nAuthorization: PH\r\n\r\n"]),
            "GET /next HTTP/1.1\r\nAuthorization: real\r\n\r\n"
        );
    }

    #[test]
    fn test_malformed_chunk_size_is_rejected() {
        let mut m = framing_manager();
        frame(
            &mut m,
            &[b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"],
        );
        assert!(m.process_write_data(1, b"zz\r\n").is_err());
    }

    /// A write reports the guest's own length, all of the (substituted) bytes
    /// reach the peer, and a write to an unknown handle leaves no state behind.
    #[tokio::test]
    async fn test_tcp_write_reports_guest_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m = framing_manager();
        let (handle, server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let handle = handle.unwrap();

        // Substitution makes the request longer on the wire than the guest's.
        let written = m.tcp_write(handle, b"GET / HTTP/1.1\r\nX: PHPH", 0).await;
        assert_eq!(written.unwrap(), 23);
        let written = m.tcp_write(handle, b"PHPH\r\n\r\n", 0).await;
        assert_eq!(written.unwrap(), 8);
        // Buffered non-HTTP bytes go out with a later write, which still
        // reports only its own length.
        assert_eq!(m.tcp_write(handle, b"hello", 0).await.unwrap(), 5);
        assert_eq!(m.tcp_write(handle, b"\r\n\r\n", 0).await.unwrap(), 4);
        m.tcp_close(handle);

        let mut received = String::new();
        let mut server = server;
        server.read_to_string(&mut received).await.unwrap();
        assert_eq!(
            received,
            "GET / HTTP/1.1\r\nX: realrealrealreal\r\n\r\nhello\r\n\r\n"
        );

        assert!(matches!(
            m.tcp_write(99, b"GET / HTTP/1.1\r\n", 0).await,
            Err(TcpError::InvalidHandle)
        ));
        assert!(m.http_states.is_empty());
    }

    /// A failed upgrade invalidates the TCP handle, so its state goes too.
    #[tokio::test]
    async fn test_failed_tls_upgrade_cleans_up_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m = framing_manager();
        let (handle, _server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let handle = handle.unwrap();
        m.tcp_write(handle, b"GET /", 0).await.unwrap();

        // An empty hostname fails before any handshake.
        assert!(m.tls_upgrade(handle, "", 0).await.is_err());
        assert_eq!(m.connection_count(), 0);
        assert!(!m.connection_hosts.contains_key(&handle));
        assert!(m.http_states.is_empty());
    }
}
