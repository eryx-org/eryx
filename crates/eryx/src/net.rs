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

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
pub use ipnet::IpNet;

mod response;
use ipnet::{Ipv4Net, Ipv6Net};
use response::{CARRY_GRACE, RequestKind, ResponseMasker, UNSOLICITED};
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

/// Largest HTTP request header block buffered while waiting for its end
/// (64 KiB). The buffer lives on the host, outside the sandbox's memory limit.
const MAX_HEADER_LEN: usize = 64 * 1024;

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
///
/// # Stability
///
/// This trait can be implemented outside this crate. Methods added to it later
/// will have default implementations, so existing implementations keep compiling.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync + std::fmt::Debug {
    /// Resolve `host` to the addresses to try, in order of preference.
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
}

/// Lets one resolver be shared between configs:
/// `config.with_resolver(Arc::clone(&resolver))`.
#[async_trait::async_trait]
impl<R: Resolver + ?Sized> Resolver for Arc<R> {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        (**self).resolve(host, port).await
    }
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
    max_connections: u32,
    /// Connection timeout for TCP connect.
    connect_timeout: Duration,
    /// Read/write timeout for I/O operations.
    io_timeout: Duration,
    /// When true, all non-blocked hosts are allowed (`allowed_hosts` is ignored).
    /// When false (default), only hosts matching `allowed_hosts` are permitted.
    allow_all_hosts: bool,
    /// Allowed host patterns (only checked when `allow_all_hosts` is false).
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`, `exact.host.com`
    allowed_hosts: Vec<String>,
    /// Blocked host patterns (checked after allowed).
    ///
    /// By default, blocks localhost and private networks.
    blocked_hosts: Vec<String>,
    /// Networks that resolved addresses must not fall in.
    ///
    /// By default, every non-public range: see [`DEFAULT_BLOCKED_NETWORKS`].
    /// IPv4-mapped IPv6 addresses are checked as IPv4.
    blocked_networks: Vec<IpNet>,
    /// Exceptions to `blocked_networks`: an address in one of these is allowed
    /// even if it also falls in a blocked network.
    allowed_networks: Vec<IpNet>,
    /// Host patterns allowed to resolve into `blocked_networks`.
    ///
    /// For trusted names that are routed internally on purpose (e.g. a
    /// service reached through cluster DNS). The host must still pass
    /// `blocked_hosts`/`allowed_hosts`. Only list names whose DNS you control:
    /// whoever answers for them decides which internal address they reach.
    internal_hosts: Vec<String>,
    /// Resolver for hostnames; `None` uses the system resolver.
    resolver: Option<Arc<dyn Resolver>>,
    /// Custom root certificates (DER-encoded) to trust in addition to system certs.
    ///
    /// Useful for testing with self-signed certificates.
    custom_root_certs: Vec<Vec<u8>>,
    /// Most bytes the execution may send and receive in all, if limited.
    max_bytes: Option<u64>,
    /// Substitute secret placeholders on plain TCP connections too.
    ///
    /// By default (`false`) placeholders are only replaced after a TLS
    /// upgrade, so a real secret never crosses the network unencrypted. A
    /// request over plain TCP (e.g. `http://`) is sent with the placeholder
    /// left in place.
    allow_plaintext_secrets: bool,
    /// Method and path rules for HTTP requests: see [`NetConfig::allow_http`].
    http_rules: Vec<HttpRule>,
}

/// The default [`NetConfig::blocked_networks`]: every range that isn't
/// publicly routable unicast.
///
/// Loopback, private (RFC 1918 and IPv6 unique-local), link-local (which
/// includes cloud metadata endpoints such as `169.254.169.254`), carrier-grade
/// NAT, "this network", IETF protocol assignments, benchmarking, multicast,
/// reserved/broadcast, local-use NAT64, 6to4, IPv4-compatible (`::/96`, which
/// includes `::` and `::1`), site-local, Teredo and documentation. Well-known
/// NAT64 (`64:ff9b::/96`), IPv4-compatible and Teredo addresses are also
/// checked by the IPv4 address they embed.
pub const DEFAULT_BLOCKED_NETWORKS: [IpNet; 20] = {
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
        v6(0, 0, 96),
        IpNet::V6(Ipv6Net::new_assert(
            Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0),
            32,
        )),
        IpNet::V6(Ipv6Net::new_assert(
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
            32,
        )),
        v6(0x2002, 0, 16),
        IpNet::V6(Ipv6Net::new_assert(
            Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0),
            48,
        )),
        v6(0xfc00, 0, 7),
        v6(0xfe80, 0, 10),
        v6(0xfec0, 0, 10),
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
            max_bytes: None,
            allow_plaintext_secrets: false,
            http_rules: vec![],
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

    /// Limit the bytes the execution may send and receive, in all, across
    /// every connection. Past it, reads and writes fail with
    /// [`TcpError::NotPermitted`].
    #[must_use]
    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// The most bytes the execution may send and receive, if limited.
    #[must_use]
    pub fn max_bytes(&self) -> Option<u64> {
        self.max_bytes
    }

    /// Allow connections to localhost (disabled by default).
    #[must_use]
    pub fn allow_localhost(mut self) -> Self {
        self.blocked_hosts
            .retain(|p| !p.contains("localhost") && !p.starts_with("127.") && !p.contains("::1"));
        self.blocked_networks.retain(|n| !n.addr().is_loopback());
        // `::1` is also inside the default `::/96` block.
        let loopback = IpNet::from(IpAddr::V6(Ipv6Addr::LOCALHOST));
        if !self.allowed_networks.contains(&loopback) {
            self.allowed_networks.push(loopback);
        }
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
            max_bytes: None,
            allow_plaintext_secrets: false,
            http_rules: vec![],
        }
    }

    /// Substitute secret placeholders on plain TCP connections as well as TLS;
    /// see [`NetConfig::allows_plaintext_secrets`].
    #[must_use]
    pub fn allow_plaintext_secrets(mut self, allow: bool) -> Self {
        self.allow_plaintext_secrets = allow;
        self
    }

    /// Only allow the HTTP requests `rule` describes to hosts matching its
    /// host pattern.
    ///
    /// A host that any rule matches only accepts requests matching one of
    /// those rules; hosts no rule matches are unaffected. Once any rule is
    /// set, every connection's requests are parsed and checked, against the
    /// connected host and each host the request names (`Host` header or
    /// absolute-form target), and ambiguous request framing is refused.
    /// Bytes that aren't HTTP/1.1 are refused on connections to a host with
    /// rules, as is a request with more than one `Host` header or a `Host`
    /// that disagrees with its absolute-form target. A refused request is
    /// never sent. Host names are compared ignoring case, a port, a trailing
    /// dot and IPv6 brackets. See [`HttpRule`] for how methods and paths
    /// match.
    ///
    /// # Limitations
    ///
    /// Setting any rule stops WebSocket traffic after an `Upgrade`, `CONNECT`
    /// tunnels and HTTP/2 on every connection that starts with an HTTP
    /// request, to any host: what follows isn't a request the host can check.
    ///
    /// Rules are enforced on what the host can frame. A first line shaped
    /// like a request (a method token, then a target starting with `/` or
    /// `*` or containing `://`, or `HTTP/` anywhere on the line) is parsed
    /// strictly, and refused unless it's a valid HTTP/1.x request line, so
    /// HTTP/0.9 is refused. A connection to a host without rules whose first
    /// line has another shape (e.g. Redis `SET k v`) is passed through
    /// unparsed from then on; one cost is that an inline command whose first
    /// argument starts with `/` is refused instead. So if
    /// sandboxed code does its own TLS to another allowed host served by the
    /// same server as a ruled one, the requests inside aren't checked. Don't
    /// allow other names on a server whose rules matter. On any other framed
    /// connection, bytes that aren't a request (including a leading empty
    /// line) are refused, and after a refusal every later write on the
    /// connection is too.
    #[must_use]
    pub fn allow_http(mut self, rule: HttpRule) -> Self {
        self.http_rules.push(rule);
        self
    }

    /// Replace the HTTP rules; see [`NetConfig::allow_http`].
    #[must_use]
    pub fn with_http_rules(mut self, rules: impl IntoIterator<Item = HttpRule>) -> Self {
        self.http_rules = rules.into_iter().collect();
        self
    }

    /// The HTTP rules; see [`NetConfig::allow_http`].
    #[must_use]
    pub fn http_rules(&self) -> &[HttpRule] {
        &self.http_rules
    }

    /// Whether any HTTP rule matches `host`.
    fn has_http_rules(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.http_rules
            .iter()
            .any(|r| host_matches_pattern(&host, &normalize_host(&r.host)))
    }

    /// Check a request with `method` and request target `target` (as in the
    /// request line: a path, or an absolute URL) against the HTTP rules for
    /// `host`. A host no rule matches allows any request.
    ///
    /// # Errors
    ///
    /// Describes the refused request.
    pub fn check_http_request(&self, host: &str, method: &str, target: &str) -> Result<(), String> {
        let host = normalize_host(host);
        let mut rules = self
            .http_rules
            .iter()
            .filter(|r| host_matches_pattern(&host, &normalize_host(&r.host)))
            .peekable();
        if rules.peek().is_none() {
            return Ok(());
        }
        let path = request_path(target);
        if let Some(path) = &path
            && rules.any(|r| r.allows(method, path))
        {
            return Ok(());
        }
        Err(format!(
            "request not allowed by HTTP rules for {host}: {method} {target}"
        ))
    }

    /// Host patterns `secret` may be sent to: its own `allowed_hosts`, or
    /// failing that this config's `allowed_hosts` unless `allow_all_hosts` is
    /// set (when that list doesn't restrict connections). Empty means the
    /// secret is never substituted.
    #[must_use]
    pub fn secret_hosts<'a>(&'a self, secret: &'a crate::secrets::SecretConfig) -> &'a [String] {
        if !secret.allowed_hosts.is_empty() {
            &secret.allowed_hosts
        } else if self.allow_all_hosts {
            &[]
        } else {
            &self.allowed_hosts
        }
    }

    /// Fail if a secret has no host it could be sent to; see
    /// [`NetConfig::secret_hosts`].
    ///
    /// [`SandboxBuilder::build`](crate::SandboxBuilder::build) runs this
    /// check when networking is enabled.
    ///
    /// # Errors
    ///
    /// Names the first such secret.
    pub fn check_secret_hosts(
        &self,
        secrets: &HashMap<String, crate::secrets::SecretConfig>,
    ) -> Result<(), String> {
        let mut names: Vec<&String> = secrets
            .iter()
            .filter(|(_, s)| self.secret_hosts(s).is_empty())
            .map(|(name, _)| name)
            .collect();
        names.sort();
        match names.first() {
            Some(name) => Err(format!(
                "secret '{name}' has no allowed hosts: give it allowed_hosts, or list the hosts \
                 in NetConfig.allowed_hosts with allow_all_hosts off"
            )),
            None => Ok(()),
        }
    }

    /// Fail if a setting can't work: a zero connect or I/O timeout would
    /// make every connection time out immediately, and an HTTP rule's path
    /// must be one a request can match.
    ///
    /// [`SandboxBuilder::build`](crate::SandboxBuilder::build) runs this
    /// check when networking is enabled.
    ///
    /// # Errors
    ///
    /// Names the first invalid setting.
    pub fn validate(&self) -> Result<(), String> {
        if self.connect_timeout.is_zero() {
            return Err("NetConfig connect_timeout must be greater than zero".to_string());
        }
        if self.io_timeout.is_zero() {
            return Err("NetConfig io_timeout must be greater than zero".to_string());
        }
        for rule in &self.http_rules {
            rule.validate()?;
        }
        let patterns = [
            ("allowed_hosts", &self.allowed_hosts),
            ("blocked_hosts", &self.blocked_hosts),
            ("internal_hosts", &self.internal_hosts),
        ]
        .into_iter()
        .flat_map(|(field, patterns)| patterns.iter().map(move |p| (field, p)))
        .chain(
            self.http_rules
                .iter()
                .map(|rule| ("an HTTP rule's host", &rule.host)),
        );
        for (field, pattern) in patterns {
            if pattern != "*" && !is_host_pattern(&normalize_host(pattern)) {
                return Err(format!(
                    "{field} pattern {pattern:?} names no host once its trailing dot is \
                     ignored"
                ));
            }
        }
        Ok(())
    }

    /// Add a custom root certificate (DER-encoded) to trust.
    ///
    /// This is useful for testing with self-signed certificates.
    #[must_use]
    pub fn with_root_cert(mut self, cert_der: impl Into<Vec<u8>>) -> Self {
        self.custom_root_certs.push(cert_der.into());
        self
    }

    /// Allow every host that isn't blocked, ignoring `allowed_hosts`; see
    /// [`NetConfig::allows_all_hosts`].
    #[must_use]
    pub fn allow_all_hosts(mut self, allow: bool) -> Self {
        self.allow_all_hosts = allow;
        self
    }

    /// Replace the allowed host patterns; see [`NetConfig::allowed_hosts`].
    #[must_use]
    pub fn with_allowed_hosts(
        mut self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.allowed_hosts = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the blocked host patterns, including the defaults; see
    /// [`NetConfig::blocked_hosts`].
    #[must_use]
    pub fn with_blocked_hosts(
        mut self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.blocked_hosts = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the blocked networks, including the defaults; see
    /// [`NetConfig::blocked_networks`].
    #[must_use]
    pub fn with_blocked_networks(mut self, networks: impl IntoIterator<Item = IpNet>) -> Self {
        self.blocked_networks = networks.into_iter().collect();
        self
    }

    /// Replace the allowed networks; see [`NetConfig::allowed_networks`].
    #[must_use]
    pub fn with_allowed_networks(mut self, networks: impl IntoIterator<Item = IpNet>) -> Self {
        self.allowed_networks = networks.into_iter().collect();
        self
    }

    /// Replace the internal host patterns; see [`NetConfig::internal_hosts`].
    #[must_use]
    pub fn with_internal_hosts(
        mut self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.internal_hosts = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Maximum concurrent connections (0 = unlimited).
    #[must_use]
    pub fn max_connections(&self) -> u32 {
        self.max_connections
    }

    /// Connection timeout for TCP connect.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Read/write timeout for I/O operations.
    #[must_use]
    pub fn io_timeout(&self) -> Duration {
        self.io_timeout
    }

    /// When true, all non-blocked hosts are allowed (`allowed_hosts` is ignored).
    /// When false (default), only hosts matching `allowed_hosts` are permitted.
    #[must_use]
    pub fn allows_all_hosts(&self) -> bool {
        self.allow_all_hosts
    }

    /// Allowed host patterns (only checked when `allow_all_hosts` is false).
    ///
    /// Patterns support wildcards: `*.example.com`, `api.*.com`, `exact.host.com`
    #[must_use]
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    /// Blocked host patterns (checked after allowed).
    ///
    /// By default, blocks localhost and private networks.
    #[must_use]
    pub fn blocked_hosts(&self) -> &[String] {
        &self.blocked_hosts
    }

    /// Networks that resolved addresses must not fall in.
    ///
    /// By default, every non-public range: see [`DEFAULT_BLOCKED_NETWORKS`].
    /// IPv4-mapped IPv6 addresses are checked as IPv4.
    #[must_use]
    pub fn blocked_networks(&self) -> &[IpNet] {
        &self.blocked_networks
    }

    /// Exceptions to `blocked_networks`: an address in one of these is allowed
    /// even if it also falls in a blocked network.
    #[must_use]
    pub fn allowed_networks(&self) -> &[IpNet] {
        &self.allowed_networks
    }

    /// Host patterns allowed to resolve into `blocked_networks`.
    ///
    /// For trusted names that are routed internally on purpose (e.g. a
    /// service reached through cluster DNS). The host must still pass
    /// `blocked_hosts`/`allowed_hosts`. Only list names whose DNS you control:
    /// whoever answers for them decides which internal address they reach.
    #[must_use]
    pub fn internal_hosts(&self) -> &[String] {
        &self.internal_hosts
    }

    /// Resolver for hostnames; `None` uses the system resolver.
    #[must_use]
    pub fn resolver(&self) -> Option<&Arc<dyn Resolver>> {
        self.resolver.as_ref()
    }

    /// Custom root certificates (DER-encoded) trusted in addition to system certs.
    #[must_use]
    pub fn custom_root_certs(&self) -> &[Vec<u8>] {
        &self.custom_root_certs
    }

    /// Whether secret placeholders are substituted on plain TCP connections
    /// too, not only after a TLS upgrade.
    ///
    /// By default (`false`) a real secret never crosses the network
    /// unencrypted: a request over plain TCP (e.g. `http://`) is sent with the
    /// placeholder left in place.
    #[must_use]
    pub fn allows_plaintext_secrets(&self) -> bool {
        self.allow_plaintext_secrets
    }
}

/// HTTP requests allowed to the hosts matching a pattern: see
/// [`NetConfig::allow_http`].
///
/// - Methods are compared ignoring case; no methods means any method.
/// - Paths match whole segments: `/repos/org/repo` matches it and anything
///   under `/repos/org/repo/`, but not `/repos/org/repo-evil`. A path ending
///   in `/` matches anything under it. No paths means any path.
/// - A request's query string is ignored and its percent-escapes decoded
///   before matching. A path with a `.` or `..` segment (escaped or not, or
///   with `;` parameters), an empty segment (`//`), an escaped `/` or `\`, or
///   a malformed escape never matches, nor does a target that isn't a path or
///   an absolute URL (`CONNECT`, `OPTIONS *`).
///
/// ```
/// use eryx::{HttpRule, NetConfig};
///
/// let config = NetConfig::default()
///     .allow_host("api.github.com")
///     .allow_http(
///         HttpRule::new("api.github.com")
///             .with_methods(["GET", "POST"])
///             .with_paths(["/repos/org/repo"]),
///     );
/// assert!(config.check_http_request("api.github.com", "GET", "/repos/org/repo/issues?page=2").is_ok());
/// assert!(config.check_http_request("api.github.com", "DELETE", "/repos/org/repo").is_err());
/// assert!(config.check_http_request("api.github.com", "GET", "/repos/org/repo-evil").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRule {
    host: String,
    methods: Vec<String>,
    paths: Vec<String>,
}

impl HttpRule {
    /// A rule for hosts matching `host` (same patterns as
    /// [`NetConfig::allow_host`]), allowing any method and path until
    /// narrowed.
    #[must_use]
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            methods: vec![],
            paths: vec![],
        }
    }

    /// Only allow these methods.
    #[must_use]
    pub fn with_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Only allow paths under these.
    #[must_use]
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// The host pattern.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The allowed methods; empty allows any.
    #[must_use]
    pub fn methods(&self) -> &[String] {
        &self.methods
    }

    /// The allowed path prefixes; empty allows any.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    fn validate(&self) -> Result<(), String> {
        for path in &self.paths {
            if request_path(path).as_deref() != Some(path) || path.contains(['?', '#']) {
                return Err(format!(
                    "HTTP rule path {path:?} for {} must be a normalized absolute path",
                    self.host
                ));
            }
        }
        Ok(())
    }

    /// Whether this rule allows `method` on the decoded `path`.
    fn allows(&self, method: &str, path: &str) -> bool {
        let method_ok =
            self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method));
        let path_ok = self.paths.is_empty()
            || self.paths.iter().any(|prefix| {
                path.strip_prefix(prefix.as_str()).is_some_and(|rest| {
                    prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/')
                })
            });
        method_ok && path_ok
    }
}

/// The decoded path of request target `target` (a path, or an absolute
/// URL's path), without its query; `None` if it has none or is ambiguous:
/// see [`HttpRule`].
fn request_path(target: &str) -> Option<String> {
    let path = if target.starts_with('/') {
        target
    } else {
        let (_, rest) = target.split_once("://")?;
        match rest.find(['/', '?', '#']) {
            Some(i) if rest[i..].starts_with('/') => &rest[i..],
            _ => "/",
        }
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();

    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                let byte = u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?;
                // An escaped separator would be read as a separator by some
                // servers and not by others.
                if matches!(byte, b'/' | b'\\') {
                    return None;
                }
                decoded.push(byte);
            }
            b'\\' => return None,
            _ => decoded.push(b),
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;

    let segments: Vec<&str> = decoded.split('/').skip(1).collect();
    for (i, segment) in segments.iter().enumerate() {
        let name = segment.split(';').next().unwrap_or_default();
        // An empty segment is only allowed last (a trailing `/`).
        if (segment.is_empty() && i + 1 < segments.len()) || name == "." || name == ".." {
            return None;
        }
    }
    Some(decoded)
}

/// `host` lowercased, without a trailing dot or IPv6 brackets, and an IP
/// address in its canonical form: the resolver accepts other spellings of
/// the same address (`2130706433`, `0x7f.1`, `0177.0.0.1`,
/// `0:0:0:0:0:0:0:1`, `::ffff:127.0.0.1`), so they must match the same
/// patterns. An IPv4-mapped IPv6 address becomes its IPv4 address.
fn normalize_host(host: &str) -> String {
    let host = host.strip_circumfix('[', ']').unwrap_or(host);
    let host = host.trim_suffix('.').to_ascii_lowercase();
    // The resolver accepts a zone (`fe80::1%lo`, `::1%1`, and with
    // systemd-resolved `203.0.113.5%lo`) after an address and connects to
    // the address, so match on the address alone.
    let addr = host
        .split_once('%')
        .map_or(host.as_str(), |(addr, _zone)| addr);
    if let Ok(v6) = addr.parse::<Ipv6Addr>() {
        return match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        };
    }
    match parse_inet_aton(addr) {
        Some(v4) => v4.to_string(),
        None => host,
    }
}

/// `host` as an IPv4 address in any form `inet_aton` (and so the system
/// resolver) accepts: one to four parts, each decimal, `0x` hex (with at
/// least one digit) or leading-`0` octal, the last filling the remaining
/// bytes.
fn parse_inet_aton(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) = if let Some(hex) = part.strip_prefix("0x") {
            (hex, 16)
        } else if part.len() > 1 && part.starts_with('0') {
            (&part[1..], 8)
        } else {
            (*part, 10)
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        values.push(u32::from_str_radix(digits, radix).ok()?);
    }
    let (last, init) = values.split_last()?;
    if init.iter().any(|&v| v > 0xff) {
        return None;
    }
    let last_bits = 8 * (4 - init.len() as u32);
    if last_bits < 32 && *last >= 1 << last_bits {
        return None;
    }
    let high = init
        .iter()
        .enumerate()
        .fold(0u32, |acc, (i, &v)| acc | (v << (24 - 8 * i)));
    Some(Ipv4Addr::from(high | last))
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
    /// Whether a request has been framed on this connection yet.
    started: bool,
    /// Whether everything on this connection is passed through unparsed
    /// from now on: its first bytes weren't a request, or a `CONNECT` or
    /// `Upgrade` without HTTP rules ended (see [`NetConfig::allow_http`]).
    raw: bool,
    /// Whether the current request is a `CONNECT` or asks for an `Upgrade`:
    /// once it ends, what follows may be another protocol, or go to another
    /// host, so it's never parsed (or substituted) as requests.
    switches: bool,
    /// Whether the one empty line allowed between requests was skipped.
    skipped_crlf: bool,
    /// Whether a write was refused, after which every write is.
    failed: bool,
    /// The kinds of the requests sent and still awaiting a response, which
    /// the read side frames those responses by.
    requests: std::collections::VecDeque<RequestKind>,
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

/// Position within a `Transfer-Encoding: chunked` body, which is parsed
/// strictly: a server more lenient than this could read the next request as
/// part of the body, and one stricter would refuse it, so anything that
/// isn't exactly the RFC 9112 grammar is refused.
#[derive(Debug)]
enum Chunked {
    /// On a chunk-size line, after `digits` hex digits giving `size`.
    Size {
        size: usize,
        digits: usize,
        part: SizePart,
    },
    /// Inside chunk data, with this many bytes left.
    Data(usize),
    /// On the `\r\n` after chunk data; `cr` once its `\r` is seen.
    DataEnd { cr: bool },
    /// In the trailer section; `line` is the current line's length so far,
    /// and `cr` whether its `\r` is seen.
    Trailer { line: usize, cr: bool },
}

/// Where a chunk-size line is, after its hex digits.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SizePart {
    /// Still in the digits.
    Digits,
    /// In whitespace after them, before a `;`.
    Space,
    /// In the chunk extensions, after a `;`.
    Ext,
    /// After the line's `\r`, before its `\n`.
    Cr,
}

impl Chunked {
    const START: Self = Self::Size {
        size: 0,
        digits: 0,
        part: SizePart::Digits,
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
                        *self = Self::DataEnd { cr: false };
                    }
                    continue;
                }
                Self::Size { size, digits, part } => match (*part, byte) {
                    (SizePart::Cr, b'\n') if *size == 0 => {
                        *self = Self::Trailer { line: 0, cr: false };
                    }
                    (SizePart::Cr, b'\n') => *self = Self::Data(*size),
                    (SizePart::Cr, _) => return Err(malformed()),
                    // A size needs at least one digit before anything else.
                    (_, _) if *digits == 0 && !byte.is_ascii_hexdigit() => {
                        return Err(malformed());
                    }
                    (_, b'\r') => *part = SizePart::Cr,
                    (_, b'\n') => return Err(malformed()),
                    (SizePart::Ext, _) => {}
                    (_, b';') => *part = SizePart::Ext,
                    (_, b' ' | b'\t') => *part = SizePart::Space,
                    (SizePart::Digits, _) => {
                        let digit = char::from(byte).to_digit(16).ok_or_else(malformed)?;
                        *size = size
                            .checked_mul(16)
                            .and_then(|s| s.checked_add(digit as usize))
                            .ok_or_else(malformed)?;
                        *digits += 1;
                    }
                    (SizePart::Space, _) => return Err(malformed()),
                },
                Self::DataEnd { cr } => match (*cr, byte) {
                    (false, b'\r') => *cr = true,
                    (true, b'\n') => *self = Self::START,
                    _ => return Err(malformed()),
                },
                Self::Trailer { line, cr } => match (*cr, byte) {
                    (true, b'\n') if *line == 0 => return Ok(Some(i + 1)),
                    (true, b'\n') => {
                        *line = 0;
                        *cr = false;
                    }
                    (false, b'\r') => *cr = true,
                    (false, b'\n') | (true, _) => return Err(malformed()),
                    (false, _) => *line += 1,
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
    /// Bytes sent and received so far, across all connections
    traffic: NetTraffic,
    /// Secrets configuration for substitution
    secrets: HashMap<String, crate::secrets::SecretConfig>,
    /// Per-connection read masking state.
    read_states: HashMap<u32, ReadState>,
}

/// Bytes an execution sent and received over the network, as the guest
/// wrote and read them (TLS and substitution don't count).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetTraffic {
    /// Bytes the guest wrote to its connections.
    pub sent: u64,
    /// Bytes the guest read from its connections.
    pub received: u64,
}

impl NetTraffic {
    fn total(self) -> u64 {
        self.sent.saturating_add(self.received)
    }
}

/// Read data held back from the guest by one connection.
#[derive(Debug, Default)]
struct ReadState {
    /// Frames and masks its responses, if any secret may be sent to its
    /// host (see [`response`]).
    masker: Option<ResponseMasker>,
    /// A read error that came with held-back bytes still to deliver: those
    /// go out first, and this on the next read.
    error: Option<std::io::Error>,
}

impl ReadState {
    fn new(masks: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Self {
            masker: ResponseMasker::new(masks),
            error: None,
        }
    }
}

/// Read up to `len` bytes from `stream`, replacing each real secret value
/// with its placeholder wherever the guest's HTTP client will see it whole
/// (see [`response`]). `http` is the connection's request framing, whose
/// requests still awaiting a response frame the responses.
///
/// What the guest gets, and when, depends only on the responses' framing
/// and on when bytes arrive, not on whether they look like a secret. The
/// read gives up after `io_timeout` in all, except that held-back bytes the
/// grace period applies to always wait all of it: `io_timeout` comes from
/// the guest, which must not be able to shorten it.
async fn read_masked<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    state: &mut ReadState,
    http: &mut HttpParsingState,
    len: u32,
    io_timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let want = read_buf_len(len);
    if want == 0 {
        return Ok(Vec::new());
    }
    let ReadState { masker, error } = state;
    let Some(masker) = masker else {
        let mut buf = vec![0u8; want];
        let n = tokio::time::timeout(io_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        buf.truncate(n);
        return Ok(buf);
    };
    let deadline = tokio::time::Instant::now() + io_timeout;
    while masker.out.is_empty() {
        if let Some(e) = error.take() {
            return Err(e);
        }
        // Responses it can't frame end the connection for the guest; an
        // unsolicited one (a 408 before an idle close) ends it cleanly.
        match masker.failure() {
            Some(UNSOLICITED) => return Ok(Vec::new()),
            Some(why) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why)),
            None => {}
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let quiet = masker.may_release_on_quiet();
        let wait = if quiet { CARRY_GRACE } else { left };
        let mut buf = vec![0u8; want];
        match tokio::time::timeout(wait, stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                masker.finish();
                if masker.out.is_empty() {
                    return Ok(Vec::new());
                }
            }
            Ok(Ok(n)) => {
                // Decided by the connection's first bytes: if its first write
                // wasn't a request, or the server spoke before anything was
                // written, it's another protocol, which nothing is ever
                // substituted on (and never will be: the write side stays
                // raw).
                if masker.at_start()
                    && !http.started
                    && http.buffer.is_empty()
                    && http.requests.is_empty()
                {
                    http.raw = true;
                }
                if masker.at_start() && http.raw && !http.started {
                    masker.raw_from_start();
                }
                masker.feed(&buf[..n], &mut http.requests);
            }
            // The held-back bytes were received: deliver them before the
            // error rather than lose them.
            Ok(Err(e)) => {
                masker.finish();
                if masker.out.is_empty() {
                    return Err(e);
                }
                *error = Some(e);
            }
            Err(_) if quiet => masker.release_on_quiet(),
            Err(_) => return Err(std::io::ErrorKind::TimedOut.into()),
        }
    }
    let rest = masker.out.split_off(want.min(masker.out.len()));
    Ok(std::mem::replace(&mut masker.out, rest))
}

/// `(real value, placeholder)` of each secret that may be sent to `host`,
/// the only secrets its responses can legitimately contain, if `substituted`
/// (requests on the connection get real values substituted): on any other
/// connection no real value was ever sent, so nothing is masked.
fn read_masks(
    config: &NetConfig,
    secrets: &HashMap<String, crate::secrets::SecretConfig>,
    host: Option<&String>,
    substituted: bool,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Some(host) = host.filter(|_| substituted) else {
        return Vec::new();
    };
    secrets
        .values()
        .filter(|s| !s.real_value.is_empty())
        .filter(|s| {
            config
                .secret_hosts(s)
                .iter()
                .any(|p| host_matches_pattern(host, p))
        })
        .map(|s| {
            (
                s.real_value.clone().into_bytes(),
                s.placeholder.clone().into_bytes(),
            )
        })
        .collect()
}

impl ConnectionManager {
    /// Bytes sent and received so far.
    #[must_use]
    pub fn traffic(&self) -> NetTraffic {
        self.traffic
    }

    /// How many more bytes the execution may move, if it's limited; `Err`
    /// once it can't move any.
    fn remaining(&self) -> Result<Option<u64>, TcpError> {
        let Some(max) = self.config.max_bytes else {
            return Ok(None);
        };
        match max.saturating_sub(self.traffic.total()) {
            0 => Err(self.over_limit()),
            left => Ok(Some(left)),
        }
    }

    fn over_limit(&self) -> TcpError {
        TcpError::NotPermitted(format!(
            "network limit reached: {} bytes sent and received",
            self.config.max_bytes.unwrap_or_default()
        ))
    }

    /// Refuse a write of `len` bytes that would go over the limit.
    fn check_write(&self, len: usize) -> Result<(), TcpError> {
        match self.remaining()? {
            Some(left) if len as u64 > left => Err(self.over_limit()),
            _ => Ok(()),
        }
    }

    /// `len` capped at what's left of the limit.
    fn read_len(&self, len: u32) -> Result<u32, TcpError> {
        Ok(match self.remaining()? {
            Some(left) => len.min(u32::try_from(left).unwrap_or(u32::MAX)),
            None => len,
        })
    }

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
            traffic: NetTraffic::default(),
            read_states: HashMap::new(),
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

    /// Run guest-written bytes through HTTP request framing, checking each
    /// request against the HTTP rules and substituting secret placeholders
    /// in its headers.
    ///
    /// This is the shared logic used by both `tcp_write()` and `tls_write()`.
    /// Returns the bytes to send now: empty while a request's headers are
    /// still incomplete.
    fn process_write_data(&mut self, handle: u32, data: &[u8]) -> Result<Vec<u8>, TcpError> {
        let mut state = self.http_states.remove(&handle).unwrap_or_default();
        if state.failed {
            self.http_states.insert(handle, state);
            return Err(TcpError::NotPermitted(
                "an earlier write on this connection was refused".into(),
            ));
        }
        let result = self.frame_requests(handle, &mut state, data);
        // After an error the buffered bytes are dropped, and the connection
        // refuses further writes: its framing can't be trusted any more.
        // The requests already sent still get their responses framed.
        if result.is_err() {
            state = HttpParsingState {
                failed: true,
                started: state.started,
                requests: std::mem::take(&mut state.requests),
                ..HttpParsingState::default()
            };
        }
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
                Body::None if state.raw => {
                    out.extend_from_slice(&input[pos..]);
                    break;
                }
                Body::None if state.switches => {
                    if pos == input.len() {
                        break;
                    }
                    if !self.config.http_rules.is_empty() {
                        return Err(TcpError::NotPermitted(
                            "nothing may follow a CONNECT or Upgrade request on this \
                             connection (HTTP rules are set)"
                                .into(),
                        ));
                    }
                    state.raw = true;
                }
                Body::None => {
                    // Only the new bytes (plus 3 for a split terminator) can
                    // complete the terminator.
                    let mut searched = state.buffer.len().saturating_sub(3);
                    state.buffer.extend_from_slice(&input[pos..]);
                    // Servers skip an empty line before a request, and some
                    // clients send one after a body, so one is passed on
                    // between requests (never before the first: see
                    // `classify`). What follows is still framed.
                    if state.started && !state.skipped_crlf {
                        if state.buffer == b"\r" {
                            break;
                        }
                        if state.buffer.starts_with(b"\r\n") {
                            out.extend(state.buffer.drain(..2));
                            state.skipped_crlf = true;
                            searched = 0;
                            if state.buffer.is_empty() {
                                break;
                            }
                        }
                    }
                    let is_request = self.classify(handle, state)?;
                    match is_request {
                        Some(true) => {}
                        Some(false) => {
                            out.append(&mut state.buffer);
                            break;
                        }
                        None => {
                            if state.buffer.len() > MAX_HEADER_LEN {
                                return Err(TcpError::NotPermitted(format!(
                                    "HTTP request line exceeds {MAX_HEADER_LEN} bytes"
                                )));
                            }
                            break;
                        }
                    }
                    let Some(header_end) =
                        find_header_end(&state.buffer[searched..]).map(|end| searched + end)
                    else {
                        if state.buffer.len() > MAX_HEADER_LEN {
                            return Err(TcpError::NotPermitted(format!(
                                "HTTP request headers exceed {MAX_HEADER_LEN} bytes"
                            )));
                        }
                        break;
                    };
                    input = state.buffer.split_off(header_end);
                    pos = 0;
                    let headers = std::mem::take(&mut state.buffer);

                    // Check for HTTP/2 (not supported with secrets)
                    if is_http2(&headers) {
                        return Err(TcpError::NotPermitted(
                            "HTTP/2 is not supported here. Use HTTP/1.1.".into(),
                        ));
                    }

                    state.body = request_body(&headers)?;
                    state.started = true;
                    state.skipped_crlf = false;
                    state.switches = switches_protocols(&headers);
                    state.requests.push_back(RequestKind::of(&headers));
                    if !self.config.http_rules.is_empty() {
                        self.check_http_rules(&headers, handle)?;
                    }
                    if self.substitutes_secrets(handle) {
                        out.extend(self.substitute_http_headers(&headers, handle)?);
                    } else {
                        out.extend_from_slice(&headers);
                    }
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

    /// Whether requests on `handle` get secret placeholders substituted: only
    /// after a TLS upgrade, unless [`NetConfig::allow_plaintext_secrets`].
    fn substitutes_secrets(&self, handle: u32) -> bool {
        !self.secrets.is_empty()
            && (self.config.allow_plaintext_secrets || self.tls_connections.contains_key(&handle))
    }

    /// Whether writes on `handle` go through HTTP request framing.
    fn frames_requests(&self, handle: u32) -> bool {
        !self.config.http_rules.is_empty() || self.substitutes_secrets(handle)
    }

    /// Whether the buffered bytes start a request (`None` until that's
    /// known).
    ///
    /// Anything a server could take for a request must be framed (and, with
    /// HTTP rules, checked): a server may read whatever follows unframed
    /// bytes as their body, so a later write parsed as a new request could
    /// have a secret substituted into what reaches the server as a body. So
    /// other bytes are passed through only by marking the connection raw,
    /// never parsed or substituted again, and with HTTP rules only if
    /// they're a connection's first and its host has no rules (otherwise
    /// they're refused). A leading empty line, which servers skip before a
    /// request, and a malformed request line are always refused.
    fn classify(
        &self,
        handle: u32,
        state: &mut HttpParsingState,
    ) -> Result<Option<bool>, TcpError> {
        let host = self
            .connection_hosts
            .get(&handle)
            .ok_or(TcpError::InvalidHandle)?;
        if state.buffer.starts_with(b"\r") || state.buffer.starts_with(b"\n") {
            return Err(TcpError::NotPermitted(
                "HTTP request starts with an empty line".into(),
            ));
        }
        match request_start(&state.buffer) {
            Some(false)
                if !self.config.http_rules.is_empty()
                    && (state.started || self.config.has_http_rules(host)) =>
            {
                Err(TcpError::NotPermitted(format!(
                    "only HTTP/1.1 requests may be sent on this connection to {host} \
                     (HTTP rules are set)"
                )))
            }
            Some(false) => {
                state.raw = true;
                Ok(Some(false))
            }
            // Refused now, not once the headers end: an HTTP/0.9 request
            // has no headers.
            Some(true) if request_line(&String::from_utf8_lossy(&state.buffer)).is_none() => Err(
                TcpError::NotPermitted("malformed HTTP/1.x request line".into()),
            ),
            other => Ok(other),
        }
    }

    /// Check the request with header block `headers` against the HTTP rules
    /// for the connected host and every host the request names.
    fn check_http_rules(&self, headers: &[u8], handle: u32) -> Result<(), TcpError> {
        let host = self
            .connection_hosts
            .get(&handle)
            .ok_or(TcpError::InvalidHandle)?;
        // `request_body` has checked that the headers are UTF-8.
        let text = String::from_utf8_lossy(headers);
        let malformed = || TcpError::NotPermitted("malformed HTTP request line".into());
        let (method, target) = request_line(&text).ok_or_else(malformed)?;
        let named = request_authorities(&text).ok_or_else(malformed)?;
        // A server could route by either of two hosts that disagree.
        let host_headers = named.len() - usize::from(is_absolute_form(target));
        if host_headers > 1
            || !named
                .windows(2)
                .all(|w| same_host(strip_port(w[0]), strip_port(w[1])))
        {
            return Err(TcpError::NotPermitted(
                "HTTP request names more than one host".into(),
            ));
        }
        for host in std::iter::once(host.as_str()).chain(named.into_iter().map(strip_port)) {
            self.config
                .check_http_request(host, method, target)
                .map_err(TcpError::NotPermitted)?;
        }
        Ok(())
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

        // A spent byte limit takes no new connections, whose lookups and
        // handshakes it wouldn't count.
        self.remaining()?;

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
    /// bytes than requested. Real secret values that appear verbatim in the
    /// data (even split across reads) are replaced with their placeholders;
    /// compressed or otherwise encoded copies are not. Only secrets allowed
    /// for this connection's host are masked, and only on a connection whose
    /// requests get real values substituted. There, responses are framed:
    /// a header block is masked whole, a chunked body after de-chunking
    /// (and re-chunked), and the last few bytes of a body (one less than the
    /// longest secret value) are held back until more of it arrives or it
    /// ends (for a close-delimited body, until the connection closes), or,
    /// between chunks and after a `101` or a `CONNECT`'s `2xx`, the stream
    /// goes quiet for 100 ms. A reply that isn't a response to a request
    /// sent, or can't be framed, fails this and every later read.
    pub async fn tcp_read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, TcpError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let len = self.read_len(len)?;
        // Only responses to requests that can carry a real value are masked:
        // anywhere else none was ever sent.
        let masks_responses = self.substitutes_secrets(handle);
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        let state = self.read_states.entry(handle).or_insert_with(|| {
            ReadState::new(read_masks(
                &self.config,
                &self.secrets,
                self.connection_hosts.get(&handle),
                masks_responses,
            ))
        });
        let http = self.http_states.entry(handle).or_default();
        let data = read_masked(stream, state, http, len, io_timeout)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;
        self.traffic.received += data.len() as u64;
        Ok(data)
    }

    /// Write to a TCP connection.
    ///
    /// If secrets are configured and [`NetConfig::allow_plaintext_secrets`] is
    /// set, this method will parse HTTP requests and substitute secret
    /// placeholders in headers before sending. Otherwise bytes are sent as
    /// written, placeholders included. With HTTP rules, requests are parsed
    /// and checked either way; see [`NetConfig::allow_http`].
    pub async fn tcp_write(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<u32, TcpError> {
        if !self.tcp_connections.contains_key(&handle) {
            return Err(TcpError::InvalidHandle);
        }
        self.check_write(data.len())?;

        // Fast path: nothing to substitute or check = no parsing
        let (accepted, written) = if !self.frames_requests(handle) {
            self.tcp_write_raw(handle, data, timeout_ms).await
        } else {
            let bytes = self.process_write_data(handle, data)?;
            if bytes.is_empty() {
                (0, Ok(()))
            } else {
                self.tcp_write_raw(handle, &bytes, timeout_ms).await
            }
        };
        // A write that times out or fails may have sent some of it: that much
        // counts against the limit.
        if let Err(e) = written {
            // Capped at the guest's write: a framed connection's bytes can
            // include earlier writes it buffered, counted with those.
            self.traffic.sent += accepted.min(data.len()) as u64;
            return Err(e);
        }
        self.traffic.sent += data.len() as u64;
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write all of `data` to a TCP connection without HTTP parsing,
    /// returning how much of it the stream accepted, finished or not.
    async fn tcp_write_raw(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> (usize, Result<(), TcpError>) {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let Some(stream) = self.tcp_connections.get_mut(&handle) else {
            return (0, Err(TcpError::InvalidHandle));
        };
        let (accepted, written) = write_counted(stream, data, io_timeout, false).await;
        let written = written.map_err(|e| match e {
            None => TcpError::TimedOut,
            Some(e) => match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            },
        });
        (accepted, written)
    }

    /// Substitute secret placeholders in HTTP headers.
    fn substitute_http_headers(&self, headers: &[u8], handle: u32) -> Result<Vec<u8>, TcpError> {
        let text = String::from_utf8_lossy(headers);
        let target_host = self
            .connection_hosts
            .get(&handle)
            .ok_or(TcpError::InvalidHandle)?;

        // The server routes by the Host header (or an absolute-form target),
        // so a request naming another host must not carry secrets.
        let names_target = request_authorities(&text)
            .is_some_and(|hosts| hosts.iter().all(|a| same_host(strip_port(a), target_host)));
        if !names_target && self.secrets.values().any(|s| text.contains(&s.placeholder)) {
            return Err(TcpError::NotPermitted(
                "Secret not allowed: request names a different host than the connection".into(),
            ));
        }

        let mut result = String::new();

        let mut lines = text.lines();
        // Request line (GET /path HTTP/1.1), passed through as is.
        if let Some(line) = lines.next() {
            result.push_str(line);
            result.push_str("\r\n");
        }
        for line in lines {
            // The blank line ending the headers is re-added below.
            if line.is_empty() {
                continue;
            }

            // Check if this is a header line (contains ':')
            if let Some(colon_pos) = line.find(':') {
                let name = &line[..colon_pos];
                let value = line[colon_pos + 1..].trim_start();

                // Substitute secrets in header value
                let substituted_value =
                    self.substitute_secrets_in_text(value, name, target_host)?;
                // A placeholder inside `Basic` credentials isn't visible to the
                // check above.
                if !names_target && substituted_value != value {
                    return Err(TcpError::NotPermitted(
                        "Secret not allowed: request names a different host than the connection"
                            .into(),
                    ));
                }
                result.push_str(&format!("{name}: {substituted_value}\r\n"));
            } else {
                result.push_str(line);
                result.push_str("\r\n");
            }
        }

        result.push_str("\r\n"); // End of headers
        Ok(result.into_bytes())
    }

    /// Substitute secret placeholders in the value `text` of the header
    /// `header`.
    ///
    /// A secret is only substituted for a host matching
    /// [`NetConfig::secret_hosts`]; a secret with no hosts there is never
    /// substituted. A secret limited to one header is only substituted there.
    ///
    /// Placeholders in `Authorization: Basic` credentials, which clients
    /// base64-encode, are substituted in the decoded `user:password` and the
    /// result re-encoded.
    fn substitute_secrets_in_text(
        &self,
        text: &str,
        header: &str,
        target_host: &str,
    ) -> Result<String, TcpError> {
        if header.eq_ignore_ascii_case("authorization")
            && let Some((scheme, token)) = text.split_once(' ')
            && scheme.eq_ignore_ascii_case("basic")
            && let Ok(decoded) = BASE64_STANDARD.decode(token.trim())
            && let Ok(credentials) = String::from_utf8(decoded)
        {
            let substituted = self.substitute_placeholders(&credentials, header, target_host)?;
            if substituted != credentials {
                return Ok(format!("Basic {}", BASE64_STANDARD.encode(substituted)));
            }
        }
        self.substitute_placeholders(text, header, target_host)
    }

    /// Substitute secret placeholders appearing verbatim in `text`, the value
    /// of the header `header`, as described in
    /// [`substitute_secrets_in_text`](Self::substitute_secrets_in_text).
    fn substitute_placeholders(
        &self,
        text: &str,
        header: &str,
        target_host: &str,
    ) -> Result<String, TcpError> {
        let mut result = text.to_string();

        for secret_config in self.secrets.values() {
            if !text.contains(&secret_config.placeholder) {
                continue;
            }

            let host_allowed = self
                .config
                .secret_hosts(secret_config)
                .iter()
                .any(|p| host_matches_pattern(target_host, p));

            if !host_allowed {
                return Err(TcpError::NotPermitted(
                    "Secret not allowed for the target host".into(),
                ));
            }
            if let Some(allowed) = &secret_config.header
                && !allowed.eq_ignore_ascii_case(header)
            {
                return Err(TcpError::NotPermitted(format!(
                    "Secret not allowed in the {header} header"
                )));
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
        self.read_states.remove(&handle);
    }

    // ========================================================================
    // TLS operations
    // ========================================================================

    /// Upgrade a TCP connection to TLS.
    ///
    /// Takes ownership of the TCP connection and performs a TLS handshake.
    /// After upgrade, the original tcp_handle is invalid.
    ///
    /// `hostname` (the SNI name the certificate is verified against) must be
    /// the host passed to [`tcp_connect`](Self::tcp_connect), ignoring ASCII
    /// case and a trailing dot.
    pub async fn tls_upgrade(
        &mut self,
        tcp_handle: u32,
        hostname: &str,
        timeout_ms: u32,
    ) -> Result<u32, TlsError> {
        // A spent byte limit takes no handshakes either; the connection stays
        // the guest's.
        self.remaining().map_err(TlsError::Tcp)?;
        let connect_timeout = Self::effective_timeout(self.config.connect_timeout, timeout_ms);

        // Remove the TCP connection (we're taking ownership). Its handle is
        // gone whether or not the handshake succeeds, so drop its state now.
        let tcp = self
            .tcp_connections
            .remove(&tcp_handle)
            .ok_or(TlsError::InvalidHandle)?;
        let host = self.connection_hosts.remove(&tcp_handle);
        self.http_states.remove(&tcp_handle);
        self.read_states.remove(&tcp_handle);

        // The certificate is checked against `hostname`, and secrets against
        // the connected host, so the two must be the same host.
        if !host.as_deref().is_some_and(|h| same_host(h, hostname)) {
            return Err(TlsError::Tcp(TcpError::NotPermitted(format!(
                "TLS hostname '{hostname}' does not match the connected host"
            ))));
        }

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
    /// bytes than requested. Real secret values that appear verbatim in the
    /// data (even split across reads) are replaced with their placeholders;
    /// compressed or otherwise encoded copies are not. Only secrets allowed
    /// for this connection's host are masked, and only on a connection whose
    /// requests get real values substituted. There, responses are framed:
    /// a header block is masked whole, a chunked body after de-chunking
    /// (and re-chunked), and the last few bytes of a body (one less than the
    /// longest secret value) are held back until more of it arrives or it
    /// ends (for a close-delimited body, until the connection closes), or,
    /// between chunks and after a `101` or a `CONNECT`'s `2xx`, the stream
    /// goes quiet for 100 ms. A reply that isn't a response to a request
    /// sent, or can't be framed, fails this and every later read.
    pub async fn tls_read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, TlsError> {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let len = self.read_len(len).map_err(TlsError::Tcp)?;
        // Only responses to requests that can carry a real value are masked:
        // anywhere else none was ever sent.
        let masks_responses = self.substitutes_secrets(handle);
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;

        let state = self.read_states.entry(handle).or_insert_with(|| {
            ReadState::new(read_masks(
                &self.config,
                &self.secrets,
                self.connection_hosts.get(&handle),
                masks_responses,
            ))
        });
        let http = self.http_states.entry(handle).or_default();
        let data = read_masked(stream, state, http, len, io_timeout)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            })?;
        self.traffic.received += data.len() as u64;
        Ok(data)
    }

    /// Write to a TLS connection.
    ///
    /// If secrets are configured, this method will parse HTTP requests and
    /// substitute secret placeholders in headers before sending. With HTTP
    /// rules, requests are parsed and checked; see [`NetConfig::allow_http`].
    pub async fn tls_write(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> Result<u32, TlsError> {
        if !self.tls_connections.contains_key(&handle) {
            return Err(TlsError::InvalidHandle);
        }
        self.check_write(data.len()).map_err(TlsError::Tcp)?;

        // Fast path: nothing to substitute or check = no parsing
        let (accepted, written) = if !self.frames_requests(handle) {
            self.tls_write_raw(handle, data, timeout_ms).await
        } else {
            let bytes = self.process_write_data(handle, data)?;
            if bytes.is_empty() {
                (0, Ok(()))
            } else {
                self.tls_write_raw(handle, &bytes, timeout_ms).await
            }
        };
        // What rustls accepted of a write that times out or fails may have
        // gone out: that much counts against the limit.
        if let Err(e) = written {
            // Capped at the guest's write: a framed connection's bytes can
            // include earlier writes it buffered, counted with those.
            self.traffic.sent += accepted.min(data.len()) as u64;
            return Err(e);
        }
        self.traffic.sent += data.len() as u64;
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write all of `data` to a TLS connection without HTTP parsing,
    /// returning how much of it rustls accepted, finished or not.
    ///
    /// Flushes too, so no TLS records stay buffered once the call returns.
    async fn tls_write_raw(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> (usize, Result<(), TlsError>) {
        let io_timeout = Self::effective_timeout(self.config.io_timeout, timeout_ms);
        let Some(stream) = self.tls_connections.get_mut(&handle) else {
            return (0, Err(TlsError::InvalidHandle));
        };
        let (accepted, written) = write_counted(stream, data, io_timeout, true).await;
        let written = written.map_err(|e| match e {
            None => TlsError::Tcp(TcpError::TimedOut),
            Some(e) => match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            },
        });
        (accepted, written)
    }

    /// Close a TLS connection.
    pub fn tls_close(&mut self, handle: u32) {
        if self.tls_connections.remove(&handle).is_some() {
            tracing::debug!(handle, "TLS connection closed");
        }
        // Clean up HTTP parsing state and host tracking
        self.http_states.remove(&handle);
        self.connection_hosts.remove(&handle);
        self.read_states.remove(&handle);
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
        self.read_states.clear();
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

/// Whether `data` may start an HTTP request, by the shape of its first
/// line: a method token, then whitespace and a target that starts with `/`
/// or `*` or contains `://`, or `HTTP/` (ignoring case) anywhere on the
/// line. `None` until its first line is complete or it can no longer be one.
///
/// Loose on purpose: anything a lenient server might take for a request
/// (HTTP/0.9 included) is framed, and `request_line` then refuses the
/// malformed ones.
fn request_start(data: &[u8]) -> Option<bool> {
    let is_space = |b: &u8| matches!(b, b' ' | b'\t');
    let method_len = data.iter().take_while(|b| is_tchar(**b)).count();
    // Only a method token and then whitespace can start a request line, so
    // anything else (e.g. a binary protocol's header) is known at once.
    match data.get(method_len) {
        None => return None,
        Some(b) if method_len > 0 && is_space(b) => {}
        Some(_) => return Some(false),
    }
    let line_end = data.iter().position(|&b| b == b'\n')?;
    let line = &data[..line_end];
    if line.windows(5).any(|w| w.eq_ignore_ascii_case(b"HTTP/")) {
        return Some(true);
    }
    let rest = &line[method_len..];
    if !rest.first().is_some_and(is_space) {
        return Some(false);
    }
    let rest = &rest[rest.iter().take_while(|b| is_space(b)).count()..];
    let target = &rest[..rest
        .iter()
        .position(|b| is_space(b) || *b == b'\r')
        .unwrap_or(rest.len())];
    Some(
        target.starts_with(b"/")
            || target.starts_with(b"*")
            || target.windows(3).any(|w| w == b"://"),
    )
}

/// Whether a request's `headers` make what follows it something other than
/// requests: a `CONNECT` (a tunnel, maybe to another host) or any `Upgrade`.
fn switches_protocols(headers: &[u8]) -> bool {
    let text = String::from_utf8_lossy(headers);
    let mut lines = text.split("\r\n");
    // Methods are case-sensitive, but a lenient proxy may take `connect`.
    let connect = lines.next().is_some_and(|line| {
        line.split(' ')
            .next()
            .is_some_and(|method| method.eq_ignore_ascii_case("CONNECT"))
    });
    connect
        || lines.any(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
        })
}

/// Whether `b` is an HTTP token character (RFC 9110 `tchar`), as in a method
/// or a header name.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The hosts a request names: its absolute-form target's authority (e.g.
/// `GET https://host/ HTTP/1.1`) and each `Host` header's value.
///
/// `None` if the request line isn't exactly `METHOD SP target SP version`,
/// the target contains whitespace or `\`, or an absolute-form target has an
/// empty authority or userinfo.
fn request_authorities(headers: &str) -> Option<Vec<&str>> {
    let (_, target) = request_line(headers)?;
    let lines = headers.lines().skip(1);
    let mut hosts = Vec::new();
    if let Some((_, rest)) = is_absolute_form(target)
        .then(|| target.split_once("://"))
        .flatten()
    {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        hosts.push(authority);
    }
    hosts.extend(lines.filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("host")
            .then(|| value.trim())
    }));
    Some(hosts)
}

/// Whether request target `target` is absolute-form (`scheme://host/...`),
/// rather than a path that may contain `://` in its query.
fn is_absolute_form(target: &str) -> bool {
    !target.starts_with('/') && target.contains("://")
}

/// The method and target of a request whose request line is exactly
/// `METHOD SP target SP HTTP/1.x`, and whose target contains no whitespace or
/// `\`.
fn request_line(headers: &str) -> Option<(&str, &str)> {
    let mut parts = headers.lines().next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || method.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || target.is_empty()
        || target.contains(|c: char| c.is_whitespace() || c == '\\')
    {
        return None;
    }
    Some((method, target))
}

/// `authority` without a trailing `:port`.
fn strip_port(authority: &str) -> &str {
    if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        return &authority[..end + 2];
    }
    match authority.split_once(':') {
        // More than one colon: a bare IPv6 address, not host:port.
        Some((host, port)) if !port.contains(':') => host,
        _ => authority,
    }
}

/// Whether two host names refer to the same host: ASCII case, a trailing dot
/// and IPv6 brackets are ignored.
fn same_host(a: &str, b: &str) -> bool {
    !a.is_empty() && normalize_host(a) == normalize_host(b)
}

/// Check if data is an HTTP/2 connection preface.
fn is_http2(data: &[u8]) -> bool {
    data.starts_with(b"PRI * HTTP/2")
}

/// How the body of the request with header block `headers` (ending in a
/// blank line) is framed.
///
/// Framing a server could read differently from this is refused, so that
/// every request it sees is one the host checked: a CR or LF outside a
/// CRLF, a folded or malformed header line, a `Transfer-Encoding` other
/// than `chunked`, both that and `Content-Length`, or more than one length.
fn request_body(headers: &[u8]) -> Result<Body, TcpError> {
    let ambiguous = |what: &str| TcpError::NotPermitted(format!("ambiguous HTTP request: {what}"));
    let text = std::str::from_utf8(headers).map_err(|_| ambiguous("headers are not UTF-8"))?;
    let lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| ambiguous("unterminated headers"))?
        .split("\r\n");
    let (mut length, mut chunked) = (None, false);
    for (i, line) in lines.enumerate() {
        if line.contains(['\r', '\n']) {
            return Err(ambiguous("bare CR or LF"));
        }
        if i == 0 {
            continue; // The request line.
        }
        let (name, value) = line
            .split_once(':')
            .filter(|(name, _)| !name.is_empty() && name.bytes().all(is_tchar))
            .ok_or_else(|| ambiguous("malformed header line"))?;
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(ambiguous("unsupported Transfer-Encoding"));
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            let len = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .then(|| value.parse::<usize>().ok())
                .flatten()
                .ok_or_else(|| ambiguous("invalid Content-Length"))?;
            if length.replace(len).is_some() {
                return Err(ambiguous("more than one Content-Length"));
            }
        }
    }
    match (chunked, length) {
        (true, Some(_)) => Err(ambiguous("both Transfer-Encoding and Content-Length")),
        (true, None) => Ok(Body::Chunked(Chunked::START)),
        (false, Some(len)) if len > 0 => Ok(Body::Length(len)),
        _ => Ok(Body::None),
    }
}

/// The IPv4 address an IPv6 address stands for on the wire, if any:
/// IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`),
/// well-known-prefix NAT64 (`64:ff9b::a.b.c.d`, RFC 6052), which a NAT64
/// gateway translates to that IPv4 destination, or the client address of a
/// Teredo address (`2001::/32`, RFC 4380: the last 32 bits, inverted).
fn embedded_ipv4(ip: IpAddr) -> Option<IpAddr> {
    let IpAddr::V6(v6) = ip else { return None };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4.into());
    }
    let [a, b, rest @ ..] = v6.segments();
    let [.., w, x, y, z] = v6.octets();
    if matches!((a, b), (0x64, 0xff9b) | (0, 0)) && rest[..4] == [0, 0, 0, 0] {
        return Some(Ipv4Addr::new(w, x, y, z).into());
    }
    if (a, b) == (0x2001, 0) {
        return Some(Ipv4Addr::new(!w, !x, !y, !z).into());
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
///
/// Both sides are compared as [`normalize_host`] leaves them: ignoring case,
/// one trailing dot and IPv6 brackets.
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // `example.com.` is the same host as `example.com`, so a blocked host
    // must not get through with a trailing dot.
    let host = normalize_host(host);
    let pattern = normalize_host(pattern);
    // A pattern that normalising empties, or turns into `*` (`*.`), names no
    // host; `NetConfig::validate` refuses them.
    if !is_host_pattern(&pattern) {
        return false;
    }
    if !pattern.contains('*') {
        return host == pattern;
    }

    // Glob matching with * as wildcard: the first part anchors at the
    // start, the last at the end, and the parts between match left to right.
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

/// Write all of `data` to `stream` (and flush it, with `flush`) within
/// `timeout`, returning how many bytes it accepted whether or not that
/// finished: `Err(None)` if it timed out.
async fn write_counted<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    data: &[u8],
    timeout: Duration,
    flush: bool,
) -> (usize, Result<(), Option<std::io::Error>>) {
    let mut accepted = 0;
    let write = async {
        while accepted < data.len() {
            match stream.write(&data[accepted..]).await? {
                0 => return Err(std::io::ErrorKind::WriteZero.into()),
                n => accepted += n,
            }
        }
        if flush {
            stream.flush().await?;
        }
        Ok(())
    };
    let written = match tokio::time::timeout(timeout, write).await {
        Ok(written) => written.map_err(Some),
        Err(_) => Err(None),
    };
    (accepted, written)
}

/// Whether a normalised host pattern names some hosts and not all: not
/// empty, and not `*` (which is only a pattern as written).
fn is_host_pattern(normalized: &str) -> bool {
    !normalized.is_empty() && normalized != "*"
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn one_resolver_can_be_shared_between_configs() {
        #[derive(Debug)]
        struct Fixed;

        #[async_trait::async_trait]
        impl super::Resolver for Fixed {
            async fn resolve(
                &self,
                _host: &str,
                port: u16,
            ) -> std::io::Result<Vec<std::net::SocketAddr>> {
                Ok(vec![([192, 0, 2, 1], port).into()])
            }
        }

        let shared: std::sync::Arc<dyn super::Resolver> = std::sync::Arc::new(Fixed);
        let a = super::NetConfig::default().with_resolver(std::sync::Arc::clone(&shared));
        let b = super::NetConfig::default().with_resolver(shared);
        assert!(a.resolver().is_some() && b.resolver().is_some());
    }

    #[test]
    fn zero_timeouts_are_rejected() {
        use std::time::Duration;

        let config = super::NetConfig::default();
        assert!(config.validate().is_ok());
        let zero_connect = config.clone().with_connect_timeout(Duration::ZERO);
        assert!(
            zero_connect
                .validate()
                .unwrap_err()
                .contains("connect_timeout")
        );
        let zero_io = config.with_io_timeout(Duration::ZERO);
        assert!(zero_io.validate().unwrap_err().contains("io_timeout"));
    }

    #[test]
    fn replacing_lists_drops_the_defaults() {
        let net: IpNet = "10.0.0.0/8".parse().unwrap();
        let config = NetConfig::default()
            .with_blocked_hosts(["evil.example"])
            .with_blocked_networks([net])
            .with_allowed_hosts(["a.example", "b.example"])
            .allow_all_hosts(true);
        assert_eq!(config.blocked_hosts(), ["evil.example"]);
        assert_eq!(config.blocked_networks(), [net]);
        assert_eq!(config.allowed_hosts(), ["a.example", "b.example"]);
        assert!(config.allows_all_hosts());
    }

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
    fn test_is_http2() {
        assert!(is_http2(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));
        assert!(!is_http2(b"GET / HTTP/1.1\r\n"));
        assert!(!is_http2(b""));
    }

    /// Body framing is read from `Content-Length` or chunked encoding, and
    /// anything a server could read differently is refused.
    #[test]
    fn request_body_framing() {
        let body = |headers: &str| request_body(headers.as_bytes());
        assert!(matches!(
            body("POST / HTTP/1.1\r\nCONTENT-LENGTH: 42\r\n\r\n"),
            Ok(Body::Length(42))
        ));
        assert!(matches!(
            body("POST / HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n"),
            Ok(Body::Chunked(_))
        ));
        assert!(matches!(
            body("GET / HTTP/1.1\r\nX-Transfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n"),
            Ok(Body::None)
        ));
        for headers in [
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length : 5\r\n\r\n",
            "POST / HTTP/1.1\r\nX-A: b\r\n Content-Length: 5\r\n\r\n",
            "GET /a HTTP/1.1\nHost: x\n\nGET /b HTTP/1.1\r\n\r\n",
            "GET /a HTTP/1.1\r\nX-A: b\rc\r\n\r\n",
            "GET /a HTTP/1.1\r\nno colon\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding\x0b: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length\x00: 5\r\n\r\n",
            "POST / HTTP/1.1\r\nX(A): b\r\n\r\n",
        ] {
            assert!(body(headers).is_err(), "{headers:?}");
        }
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Test substitution for allowed host
        let result = manager.substitute_secrets_in_text(
            "Bearer ERYX_SECRET_PLACEHOLDER_abc123",
            "Authorization",
            "api.example.com",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer real-secret-value");

        // Test rejection for disallowed host
        let result = manager.substitute_secrets_in_text(
            "Bearer ERYX_SECRET_PLACEHOLDER_abc123",
            "Authorization",
            "evil.com",
        );
        assert!(result.is_err());
    }

    /// A secret limited to one header is only substituted there, whatever
    /// the header's case, and a request with it elsewhere is refused.
    #[test]
    fn secret_limited_to_a_header() {
        let secret = crate::secrets::SecretConfig {
            placeholder: "PH_HEADER".to_string(),
            ..crate::secrets::SecretConfig::new("real-secret-value", vec!["api.example.com".into()])
                .only_in_header("X-Api-Key")
        };
        let manager = ConnectionManager::new(
            NetConfig::permissive(),
            HashMap::from([("KEY".into(), secret)]),
        );

        assert_eq!(
            manager
                .substitute_secrets_in_text("PH_HEADER", "x-api-key", "api.example.com")
                .unwrap(),
            "real-secret-value"
        );
        assert!(
            manager
                .substitute_secrets_in_text("Bearer PH_HEADER", "Authorization", "api.example.com")
                .is_err()
        );
    }

    /// Placeholders inside base64 `Basic` credentials are substituted and
    /// re-encoded, under the same host and header rules.
    #[test]
    fn secret_in_basic_credentials() {
        let basic = |s: &str| format!("Basic {}", BASE64_STANDARD.encode(s));
        let mut m = framing_manager();

        assert_eq!(
            m.substitute_secrets_in_text(&basic("git:PH"), "authorization", "api.example.com")
                .unwrap(),
            basic("git:real")
        );
        assert!(
            m.substitute_secrets_in_text(&basic("git:PH"), "Authorization", "evil.com")
                .is_err()
        );
        // Other headers and schemes aren't decoded.
        let encoded = BASE64_STANDARD.encode("git:PH");
        assert_eq!(
            m.substitute_secrets_in_text(&format!("Basic {encoded}"), "X-Auth", "api.example.com")
                .unwrap(),
            format!("Basic {encoded}")
        );
        // Credentials without a placeholder are passed through as written.
        assert_eq!(
            m.substitute_secrets_in_text("basic Z2l0OnB3", "Authorization", "api.example.com")
                .unwrap(),
            "basic Z2l0OnB3"
        );

        // A request naming another host is refused.
        let request = format!(
            "GET / HTTP/1.1\r\nHost: other.example.com\r\nAuthorization: {}\r\n\r\n",
            basic("git:PH")
        );
        assert!(m.process_write_data(1, request.as_bytes()).is_err());
        m.http_states.clear();

        // A secret limited to another header isn't sent in Basic credentials.
        m.secrets.get_mut("KEY").unwrap().header = Some("X-Api-Key".into());
        assert!(
            m.substitute_secrets_in_text(&basic("git:PH"), "Authorization", "api.example.com")
                .is_err()
        );
    }

    /// A secret without hosts of its own uses the network allowlist, and is
    /// never substituted when that doesn't restrict anything.
    #[test]
    fn secret_without_hosts_is_not_sent_anywhere() {
        let secret = |hosts: Vec<String>| {
            HashMap::from([(
                "API_KEY".to_string(),
                crate::secrets::SecretConfig {
                    real_value: "real-secret-value".to_string(),
                    placeholder: "PLACEHOLDER_XYZ".to_string(),
                    allowed_hosts: hosts,
                    header: None,
                },
            )])
        };

        let permissive = NetConfig::permissive();
        assert!(permissive.check_secret_hosts(&secret(vec![])).is_err());
        let manager = ConnectionManager::new(permissive, secret(vec![]));
        assert!(
            manager
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "any-host.com"
                )
                .is_err()
        );

        // An allowlist that's ignored for connections doesn't count either.
        let allow_all = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default().allow_host("api.example.com")
        };
        assert!(allow_all.check_secret_hosts(&secret(vec![])).is_err());

        let allowlist = NetConfig::default().allow_host("api.example.com");
        assert!(allowlist.check_secret_hosts(&secret(vec![])).is_ok());
        assert!(
            NetConfig::permissive()
                .check_secret_hosts(&secret(vec!["api.example.com".into()]))
                .is_ok()
        );
        let manager = ConnectionManager::new(allowlist, secret(vec![]));
        assert_eq!(
            manager
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "api.example.com"
                )
                .unwrap(),
            "Bearer real-secret-value"
        );
        assert!(
            manager
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "other.example.com"
                )
                .is_err()
        );
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // All of these unauthorized hosts must be rejected
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "evil.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "127.0.0.1")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "localhost")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "")
                .is_err()
        );
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
                header: None,
            },
        );
        secrets.insert(
            "GITHUB".to_string(),
            crate::secrets::SecretConfig {
                real_value: "github-secret".to_string(),
                placeholder: "PH_GITHUB".to_string(),
                allowed_hosts: vec!["api.github.com".to_string()],
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // OpenAI key for OpenAI host: OK
        assert!(
            manager
                .substitute_secrets_in_text("PH_OPENAI", "Authorization", "api.openai.com")
                .is_ok()
        );
        // OpenAI key for GitHub host: REJECTED
        assert!(
            manager
                .substitute_secrets_in_text("PH_OPENAI", "Authorization", "api.github.com")
                .is_err()
        );
        // GitHub key for GitHub host: OK
        assert!(
            manager
                .substitute_secrets_in_text("PH_GITHUB", "Authorization", "api.github.com")
                .is_ok()
        );
        // GitHub key for OpenAI host: REJECTED
        assert!(
            manager
                .substitute_secrets_in_text("PH_GITHUB", "Authorization", "api.openai.com")
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "Authorization", "api.openai.com")
                .is_ok()
        );
        // Bare domain doesn't match *.openai.com
        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "Authorization", "openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_GLOB", "Authorization", "evil.com")
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Deceptive domains that look similar but shouldn't match
        assert!(
            manager
                .substitute_secrets_in_text(
                    "PH_DECEPTIVE",
                    "Authorization",
                    "api.openai.com.evil.com"
                )
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "Authorization", "api-openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "Authorization", "evil-api.openai.com")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_DECEPTIVE", "Authorization", "xapi.openai.com")
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // IP address should NOT match hostname restriction
        assert!(
            manager
                .substitute_secrets_in_text("PH_IP", "Authorization", "104.18.7.192")
                .is_err()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_IP", "Authorization", "1.2.3.4")
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // DNS is case-insensitive, so these should all work
        assert!(
            manager
                .substitute_secrets_in_text("PH_CASE", "Authorization", "API.OPENAI.COM")
                .is_ok()
        );
        assert!(
            manager
                .substitute_secrets_in_text("PH_CASE", "Authorization", "Api.OpenAI.Com")
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
                header: None,
            },
        );

        let manager = ConnectionManager::new(config, secrets);

        // Text without any placeholder should pass through unchanged, for any host
        let result = manager.substitute_secrets_in_text(
            "Bearer some-other-token",
            "Authorization",
            "evil.com",
        );
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
                header: None,
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
                header: None,
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
                header: None,
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
                header: None,
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
            "::a00:1",
            "::8.8.8.8",
            "fec0::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
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

    /// IPv4-compatible and Teredo addresses are checked as the IPv4 address
    /// they embed, like NAT64.
    #[test]
    fn ipv4_compatible_and_teredo_match_embedded_ipv4() {
        let config = NetConfig {
            blocked_networks: vec!["10.0.0.0/8".parse().unwrap()],
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new());
        assert!(!manager.is_ip_allowed(ip("::10.0.0.1")));
        // Teredo stores the client address inverted: !10.0.0.1 = f5ff:fffe.
        assert!(!manager.is_ip_allowed(ip("2001:0:1:2:3:4:f5ff:fffe")));
        assert!(manager.is_ip_allowed(ip("::8.8.8.8")));
        assert!(manager.is_ip_allowed(ip("2001:0:1:2:3:4:f7f7:f7f7")));
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
                allowed_hosts: vec!["api.example.com".to_string(), "127.0.0.1".to_string()],
                header: None,
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

    /// Without HTTP rules too, bytes the framing can't read as a request
    /// never let a later write be parsed as a new one: the server may read
    /// that write as the earlier request's body, so a secret substituted
    /// into its "headers" would reach the server outside any header.
    #[test]
    fn unframed_writes_never_lead_to_substitution() {
        let smuggled: &[u8] = b"GET / HTTP/1.1\r\nHost: api.example.com\r\nX: &text=PH\r\n\r\n";
        let post = format!(
            "POST /x HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: {}\r\n\r\n",
            smuggled.len()
        );

        // A leading empty line is refused, and so is the connection after it.
        let mut m = framing_manager();
        let preceded = format!("\r\n{post}");
        assert!(m.process_write_data(1, preceded.as_bytes()).is_err());
        assert!(m.process_write_data(1, smuggled).is_err());
        let mut m = framing_manager();
        assert!(m.process_write_data(1, b"\n").is_err());

        // Bytes that aren't a request pass through, and nothing after them is
        // parsed or substituted.
        for first in [
            &b"\x16\x03\x01"[..],
            b"PING\r\n",
            b"SET k v\r\n",
            b"GET key\r\n",
        ] {
            let mut m = framing_manager();
            assert_eq!(m.process_write_data(1, first).unwrap(), first);
            assert_eq!(m.process_write_data(1, smuggled).unwrap(), smuggled);
        }

        // A request with a method outside the usual ones is framed like any
        // other, so its body is passed through as a body.
        let mut m = framing_manager();
        let propfind = post.replacen("POST", "PROPFIND", 1);
        assert_eq!(
            frame(&mut m, &[propfind.as_bytes(), smuggled]),
            format!("{propfind}{}", String::from_utf8_lossy(smuggled))
        );

        // A malformed request line is refused.
        for line in [&b"GET /a b HTTP/1.1\r\n\r\n"[..], b"GET / HTTP/2.0\r\n\r\n"] {
            let mut m = framing_manager();
            assert!(m.process_write_data(1, line).is_err(), "{line:?}");
        }
    }

    /// A complete request with a body in one write, then another request:
    /// both get their headers substituted, and the body is left alone.
    #[test]
    fn framed_requests_with_bodies_keep_substituting() {
        let mut m = framing_manager();
        let post = b"POST / HTTP/1.1\r\nAuthorization: PH\r\nContent-Length: 2\r\n\r\nPH";
        assert_eq!(
            frame(&mut m, &[post, GET]),
            format!(
                "POST / HTTP/1.1\r\nAuthorization: real\r\nContent-Length: 2\r\n\r\nPH{GET_OUT}"
            )
        );
    }

    /// A binary protocol's first bytes go out at once, whatever byte they
    /// start with, rather than waiting for a newline (and the connection
    /// stays unparsed).
    #[test]
    fn binary_protocols_are_not_held_back() {
        for first in [
            &b"AMQP\x00\x00\x09\x01"[..],
            b"0\x0c\x02\x01\x01",
            b"\x16\x03\x01\x02\x00",
            b"GET\x00",
        ] {
            let mut m = framing_manager();
            assert_eq!(m.process_write_data(1, first).unwrap(), first, "{first:?}");
            let later = b"GET / HTTP/1.1\r\nAuthorization: PH\r\n\r\n";
            assert_eq!(m.process_write_data(1, later).unwrap(), later, "{first:?}");
        }
        // A method token that may still be followed by a space waits.
        let mut m = framing_manager();
        assert_eq!(frame(&mut m, &[b"GET"]), "");
        assert_eq!(
            frame(&mut m, &[b" / HTTP/1.1\r\nAuthorization: PH\r\n\r\n"]),
            GET_OUT
        );
    }

    /// A chunked body is framed exactly by RFC 9112: a size line without a
    /// hex digit, with junk after its digits, or with a bare CR or LF, and
    /// anything but CRLF after chunk data or ending a trailer line, are
    /// refused, so a server can't read the next request as part of the body.
    #[test]
    fn chunked_bodies_are_framed_strictly() {
        let head: &[u8] = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        for body in [
            &b"\r\n"[..],
            b" 4\r\nabcd\r\n0\r\n\r\n",
            b";4\r\nabcd\r\n0\r\n\r\n",
            b"4 4\r\nabcd\r\n0\r\n\r\n",
            b"4x\r\nabcd\r\n0\r\n\r\n",
            b"4\nabcd\r\n0\r\n\r\n",
            b"4\rxabcd\r\n0\r\n\r\n",
            b"4;a\nb\r\nabcd\r\n0\r\n\r\n",
            b"4\r\nabcdXY0\r\n\r\n",
            b"4\r\nabcd\n0\r\n\r\n",
            b"4\r\nabcd\r\r\n0\r\n\r\n",
            b"0\r\nX-T: a\nX-U: b\r\n\r\n",
            b"0\r\n\n",
        ] {
            let mut m = framing_manager();
            assert!(m.process_write_data(1, head).is_ok());
            assert!(m.process_write_data(1, body).is_err(), "{body:?}");
        }
        // Whitespace before an extension is fine, and so is a trailer.
        let mut m = framing_manager();
        let body = "4 ;ext=1\r\nabcd\r\n0\r\nX-T: PH\r\n\r\n";
        assert_eq!(
            frame(&mut m, &[head, body.as_bytes(), GET]),
            format!("{}{body}{GET_OUT}", String::from_utf8_lossy(head))
        );
    }

    /// After a `CONNECT` or a request asking for an `Upgrade`, what follows
    /// may be tunnelled to another host or be another protocol, so it's
    /// never parsed as requests: passed through as it is without rules, and
    /// refused with them.
    #[test]
    fn nothing_after_connect_or_upgrade_is_substituted() {
        let later: &[u8] = b"GET / HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: PH\r\n\r\n";
        for request in [
            &b"CONNECT evil.example:80 HTTP/1.1\r\nHost: evil.example:80\r\n\r\n"[..],
            b"connect evil.example:80 HTTP/1.1\r\nHost: evil.example:80\r\n\r\n",
            b"GET /chat HTTP/1.1\r\nHost: api.example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            b"POST / HTTP/1.1\r\nupgrade: h2c\r\nContent-Length: 2\r\n\r\nhi",
        ] {
            let mut m = framing_manager();
            assert_eq!(m.process_write_data(1, request).unwrap(), request);
            assert_eq!(m.process_write_data(1, later).unwrap(), later);
            // In the same write too.
            let mut m = framing_manager();
            let both = [request, later].concat();
            assert_eq!(m.process_write_data(1, &both).unwrap(), both);
        }

        let mut m = ConnectionManager::new(github_rules(), HashMap::new());
        m.connection_hosts.insert(2, "www.github.com".into());
        let upgrade = b"GET /ok HTTP/1.1\r\nHost: www.github.com\r\nUpgrade: websocket\r\n\r\n";
        assert!(m.process_write_data(2, upgrade).is_ok());
        assert!(m.process_write_data(2, b"\x81\x05hello").is_err());
    }

    /// One empty line between requests, which servers skip and some clients
    /// send after a body, is passed on, and the next request is still
    /// framed; a second one is refused.
    #[test]
    fn one_empty_line_between_requests_is_allowed() {
        let post: &[u8] = b"POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nx";
        let posted = "POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nx";
        let mut m = framing_manager();
        assert_eq!(
            frame(&mut m, &[&[post, b"\r\n"].concat(), GET]),
            format!("{posted}\r\n{GET_OUT}")
        );
        let mut m = framing_manager();
        assert_eq!(
            frame(&mut m, &[post, b"\r", &[b"\n", GET].concat()]),
            format!("{posted}\r\n{GET_OUT}")
        );
        let mut m = framing_manager();
        assert!(
            m.process_write_data(1, &[post, b"\r\n\r\n"].concat())
                .is_err()
        );
        // Before the first request it's still refused.
        let mut m = framing_manager();
        assert!(m.process_write_data(1, &[b"\r\n", GET].concat()).is_err());
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

    #[test]
    fn same_host_ignores_case_trailing_dot_and_brackets() {
        assert!(same_host("API.example.com", "api.example.com."));
        assert!(same_host("[::1]", "::1"));
        assert!(!same_host("api.example.com", "example.com"));
        assert!(!same_host("", ""));
        assert_eq!(strip_port("example.com:443"), "example.com");
        assert_eq!(strip_port("[::1]:443"), "[::1]");
        assert_eq!(strip_port("::1"), "::1");
    }

    /// A write that times out reports how much the stream had accepted, so
    /// it can be counted; one that finishes reports all of it.
    #[tokio::test]
    async fn write_counted_reports_what_was_accepted() {
        let (mut stream, _peer) = tokio::io::duplex(1024);
        let data = vec![b'x'; 4096];
        let (accepted, written) =
            super::write_counted(&mut stream, &data, Duration::from_millis(50), true).await;
        assert_eq!(accepted, 1024);
        assert!(matches!(written, Err(None)));

        let (mut stream, _peer) = tokio::io::duplex(8192);
        let (accepted, written) =
            super::write_counted(&mut stream, &data, Duration::from_secs(5), true).await;
        assert_eq!(accepted, 4096);
        assert!(written.is_ok());
    }

    /// A TCP write that times out counts what was sent against the byte
    /// limit, not nothing, and once the limit is spent a new connection and
    /// a TLS handshake are refused like reads and writes, leaving the
    /// connection with the guest.
    #[tokio::test]
    async fn timed_out_writes_count_and_a_spent_limit_refuses_connects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = NetConfig::permissive().with_max_bytes(64 << 20);
        let mut m = ConnectionManager::new(config, HashMap::new());
        // The peer never reads, so the write stalls once the socket buffers
        // are full.
        let (handle, _server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let handle = handle.unwrap();
        let err = m
            .tcp_write(handle, &vec![b'x'; 32 << 20], 200)
            .await
            .unwrap_err();
        assert!(matches!(err, TcpError::TimedOut), "{err}");
        let sent = m.traffic().sent;
        assert!(sent > 0 && sent < 32 << 20, "{sent}");

        m.traffic.sent = 64 << 20;
        let limit = |e: &TcpError| matches!(e, TcpError::NotPermitted(m) if m.contains("network limit reached"));
        let err = m.tcp_connect("127.0.0.1", port, 0).await.unwrap_err();
        assert!(limit(&err), "{err}");
        let err = m.tls_upgrade(handle, "127.0.0.1", 0).await.unwrap_err();
        assert!(matches!(&err, TlsError::Tcp(e) if limit(e)), "{err}");
        assert!(m.tcp_connections.contains_key(&handle));
    }

    /// A trailing dot names the same host (`pastebin.com.` is
    /// `pastebin.com`), so every host check treats the two alike: blocked
    /// hosts stay blocked, and allowed, internal and secret hosts still match.
    #[tokio::test]
    async fn trailing_dot_names_the_same_host_in_every_check() {
        let config = NetConfig::permissive()
            .block_host("pastebin.com")
            .block_host("*.internal.corp")
            .block_host("Evil.Example.");
        let m = ConnectionManager::new(config, HashMap::new());
        for host in [
            "pastebin.com",
            "pastebin.com.",
            "PASTEBIN.COM.",
            "db.internal.corp.",
            "evil.example",
            "evil.example.",
        ] {
            assert!(m.check_host_allowed(host).is_err(), "{host} got through");
        }
        assert!(m.check_host_allowed("example.com.").is_ok());

        let config = NetConfig::default()
            .allow_host("api.example.com")
            .allow_host("*.github.com");
        let m = ConnectionManager::new(config, HashMap::new());
        for host in ["api.example.com.", "API.github.com."] {
            assert!(m.check_host_allowed(host).is_ok(), "{host} refused");
        }
        assert!(m.check_host_allowed("example.com.").is_err());

        // Secrets go to their allowed hosts with or without the dot.
        let m = framing_manager();
        assert!(
            m.substitute_placeholders("PH", "authorization", "api.example.com.")
                .is_ok()
        );
        assert!(
            m.substitute_placeholders("PH", "authorization", "other.example.com.")
                .is_err()
        );

        // An internal host skips the network checks with the dot too.
        let config = NetConfig::default()
            .allow_host("svc.internal")
            .with_internal_hosts(["svc.internal"])
            .with_resolver(Fixed);
        let m = ConnectionManager::new(config, HashMap::new());
        assert!(m.resolve_allowed("svc.internal.", 443).await.is_ok());
    }

    /// Other spellings of an IP address (the forms the resolver accepts)
    /// match the patterns its canonical form does.
    #[test]
    fn numeric_hosts_match_in_canonical_form() {
        let config = NetConfig::permissive()
            .block_host("127.0.0.1")
            .block_host("10.*")
            .block_host("::1")
            .block_host("[fe80::1]");
        let m = ConnectionManager::new(config, HashMap::new());
        for host in [
            "127.0.0.1",
            "2130706433",
            "0x7f000001",
            "0x7f.1",
            "0177.0.0.1",
            "127.1",
            "127.0.1",
            "0x7F.0.0.01",
            "127.0.0.1.",
            "::ffff:127.0.0.1",
            "[::ffff:7f00:1]",
            "0xa.0.0.1",
            "167772161",
            "012.1.2.3",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
            "0000::0001",
            "FE80:0:0::1",
            "::1%1",
            "[::1%lo]",
            "::ffff:127.0.0.1%1",
            "fe80::1%lo",
            "fe80::1%25eth0",
            "127.0.0.1%lo",
        ] {
            assert!(m.check_host_allowed(host).is_err(), "{host} got through");
        }
        for host in [
            "127.0.0.2",
            "11.0.0.1",
            "::2",
            "example.com",
            "1.2.3.4.5",
            "08.1.2.3",
        ] {
            assert!(m.check_host_allowed(host).is_ok(), "{host} refused");
        }
        for (host, ip) in [
            ("2130706433", "127.0.0.1"),
            ("0x7f.1", "127.0.0.1"),
            ("0177.0.0.1", "127.0.0.1"),
            ("1.2.3", "1.2.0.3"),
            ("0x0", "0.0.0.0"),
            ("fe80::1%lo", "fe80::1"),
            ("::ffff:127.0.0.1%1", "127.0.0.1"),
            ("203.0.113.5%lo", "203.0.113.5"),
            ("::ffff:10.0.0.1", "10.0.0.1"),
            ("0:0:0:0:0:0:0:1", "::1"),
        ] {
            assert_eq!(normalize_host(host), ip, "{host}");
        }
        for host in [
            "256.1.1.1",
            "1.256.1.1",
            "1.2.65536",
            "4294967296",
            "1..2",
            "0xg",
            "09",
            "0x",
            "0x.1",
            "0x7f.0x",
            "example.com%lo",
        ] {
            assert_eq!(normalize_host(host), host, "{host}");
        }
    }

    /// A pattern that's empty or only a wildcard once its trailing dot is
    /// ignored (`.`, `*.`) names no host: it matches none, and a config
    /// with one doesn't validate. Bare `*` still matches every host.
    #[test]
    fn degenerate_host_patterns_name_no_host() {
        for pattern in ["", ".", "*.", "[]", "[*.]"] {
            assert!(!host_matches_pattern("example.com", pattern), "{pattern:?}");
            for config in [
                NetConfig::default().allow_host(pattern),
                NetConfig::default().block_host(pattern),
                NetConfig::default().with_internal_hosts([pattern]),
                NetConfig::default().allow_http(HttpRule::new(pattern)),
            ] {
                assert!(config.validate().is_err(), "{pattern:?}");
            }
        }
        assert!(host_matches_pattern("example.com", "*"));
        assert!(NetConfig::default().allow_host("*").validate().is_ok());
    }

    /// Wildcard patterns with a trailing dot match like their undotted form,
    /// `::1` and `[::1]` are the same host, and a host not in
    /// `internal_hosts` doesn't skip the network checks, dot or no dot.
    #[tokio::test]
    async fn host_pattern_spellings_match_alike() {
        let config = NetConfig::permissive()
            .block_host("*.internal.corp.")
            .block_host("api.*.com.");
        let m = ConnectionManager::new(config, HashMap::new());
        for host in [
            "db.internal.corp",
            "db.internal.corp.",
            "api.x.com",
            "api.x.com.",
        ] {
            assert!(m.check_host_allowed(host).is_err(), "{host} got through");
        }
        assert!(m.check_host_allowed("internal.corp").is_ok());

        assert!(same_host("::1", "[::1]"));
        assert!(host_matches_pattern("[::1]", "::1"));
        assert!(host_matches_pattern("::1", "[::1]"));

        let config = NetConfig::default()
            .allow_host("svc.internal")
            .allow_host("other.internal")
            .with_internal_hosts(["svc.internal"])
            .with_resolver(Fixed);
        let m = ConnectionManager::new(config, HashMap::new());
        for host in ["other.internal", "other.internal.", "svc.internal.evil"] {
            assert!(m.resolve_allowed(host, 443).await.is_err(), "{host}");
        }
    }

    /// Resolves every name to a loopback address, which the default
    /// network blocklist refuses.
    #[derive(Debug)]
    struct Fixed;

    #[async_trait::async_trait]
    impl Resolver for Fixed {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        }
    }

    /// The certificate must be checked for the host the connection was made
    /// to, so a different TLS name is refused before any handshake.
    #[tokio::test]
    async fn tls_hostname_must_match_connect_host() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m = framing_manager();
        let (handle, _server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let err = m
            .tls_upgrade(handle.unwrap(), "api.example.com", 0)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, TlsError::Tcp(TcpError::NotPermitted(m)) if m.contains("does not match")),
            "{err}"
        );
        assert_eq!(m.connection_count(), 0);
    }

    /// A request carrying a placeholder must name the connected host, in its
    /// `Host` header and in an absolute-form target.
    #[test]
    fn host_header_must_match_connect_host() {
        let mut m = framing_manager();
        for request in [
            &b"GET / HTTP/1.1\r\nHost: other.example.com\r\nAuthorization: PH\r\n\r\n"[..],
            b"GET https://other.example.com/ HTTP/1.1\r\nAuthorization: PH\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: api.example.com\r\nHost: other.example.com\r\nAuthorization: PH\r\n\r\n",
            b"GET https://evil.com\\@api.example.com/ HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: PH\r\n\r\n",
            b"GET https://user@api.example.com/ HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: PH\r\n\r\n",
            b"GET  / HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: PH\r\n\r\n",
            b"GET https:/// HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: PH\r\n\r\n",
        ] {
            assert!(m.process_write_data(1, request).is_err());
            m.http_states.clear();
        }

        let ok = frame(
            &mut m,
            &[b"GET https://api.example.com:443/x HTTP/1.1\r\nHost: API.Example.com.:443\r\nAuthorization: PH\r\n\r\n"],
        );
        assert!(ok.contains("Authorization: real"), "{ok}");
        // The request line is passed through, not parsed as a header.
        assert!(
            ok.starts_with("GET https://api.example.com:443/x HTTP/1.1\r\n"),
            "{ok}"
        );
        // Without a placeholder, nothing is sent that the server shouldn't see.
        let plain = frame(
            &mut m,
            &[b"GET / HTTP/1.1\r\nHost: other.example.com\r\n\r\n"],
        );
        assert!(plain.starts_with("GET / HTTP/1.1"), "{plain}");
    }

    fn github_rules() -> NetConfig {
        NetConfig::permissive().allow_http(
            HttpRule::new("api.github.com")
                .with_methods(["get", "POST"])
                .with_paths(["/repos/org/repo", "/user/"]),
        )
    }

    /// Paths match on segment boundaries after decoding, and anything a
    /// server could resolve elsewhere is refused.
    #[test]
    fn http_rules_match_methods_and_paths() {
        let config = github_rules();
        let check = |method, target| config.check_http_request("api.github.com", method, target);
        for target in [
            "/repos/org/repo",
            "/repos/org/repo/",
            "/repos/org/repo/issues?state=open",
            "/repos/org/%72epo/pulls",
            "/user/keys",
            "https://api.github.com/repos/org/repo#x",
        ] {
            assert!(check("GET", target).is_ok(), "{target}");
        }
        for target in [
            "/repos/org/repo-evil",
            "/repos/org",
            "/user",
            "/repos/org/repo/../../other",
            "/repos/org/repo/%2e%2E/x",
            "/repos/org/repo/..;/x",
            "/repos/org/repo/./x",
            "/repos/org/repo//x",
            "//repos/org/repo",
            "/repos/org/repo%2fx",
            "/repos/org/repo%5Cx",
            "/repos/org/repo\\x",
            "/repos/org/repo/%zz",
            "/repos/org/repo/%2",
            "/repos/org/repo/%e2",
            "*",
            "api.github.com:443",
            "https://api.github.com",
        ] {
            assert!(check("GET", target).is_err(), "{target}");
        }
        assert!(check("post", "/repos/org/repo").is_ok());
        assert!(check("DELETE", "/repos/org/repo").is_err());
        // Other hosts are unaffected; host names are normalized.
        assert!(
            config
                .check_http_request("example.com", "DELETE", "/x")
                .is_ok()
        );
        assert!(
            config
                .check_http_request("API.GitHub.com.", "DELETE", "/x")
                .is_err()
        );
    }

    /// Each rule shape allows what it describes and nothing else, and host
    /// names are compared normalized.
    #[test]
    fn http_rule_shapes() {
        let check = |rule: HttpRule, host, method, target| {
            NetConfig::permissive()
                .allow_http(rule)
                .check_http_request(host, method, target)
                .is_ok()
        };
        let h = "api.example.com";
        // Any method and path: only the host is restricted.
        assert!(check(HttpRule::new(h), h, "DELETE", "/x"));
        assert!(!check(HttpRule::new(h), h, "DELETE", "/../x"));
        // Methods only.
        let methods = || HttpRule::new(h).with_methods(["GET"]);
        assert!(check(methods(), h, "GET", "/anything"));
        assert!(!check(methods(), h, "PUT", "/anything"));
        // Paths only.
        let paths = || HttpRule::new(h).with_paths(["/v1"]);
        assert!(check(paths(), h, "PUT", "/v1/x"));
        assert!(!check(paths(), h, "PUT", "/v10"));
        // Both.
        let both = || HttpRule::new(h).with_methods(["GET"]).with_paths(["/v1"]);
        assert!(check(both(), h, "GET", "/v1"));
        assert!(!check(both(), h, "POST", "/v1"));
        assert!(!check(both(), h, "GET", "/v2"));
        // A trailing `/` matches below it but not the bare path.
        let dir = || HttpRule::new(h).with_paths(["/v1/"]);
        assert!(check(dir(), h, "GET", "/v1/x"));
        assert!(!check(dir(), h, "GET", "/v1"));
        // The root matches every path.
        assert!(check(HttpRule::new(h).with_paths(["/"]), h, "GET", "/a/b"));
        // Wildcard hosts.
        let wild = || HttpRule::new("*.example.com").with_paths(["/v1"]);
        assert!(!check(wild(), "API.Example.com.", "GET", "/v2"));
        assert!(check(wild(), "api.example.com", "GET", "/v1/x"));
        assert!(check(wild(), "example.org", "GET", "/v2"));
        // IPv6 literals match with or without brackets.
        for (pattern, host) in [("[::1]", "::1"), ("::1", "[::1]")] {
            assert!(!check(
                HttpRule::new(pattern).with_paths(["/v1"]),
                host,
                "GET",
                "/v2"
            ));
        }
    }

    /// With rules, every request a server could see on a framed connection
    /// is parsed and checked, whatever its method, and a refusal sticks.
    #[test]
    fn http_rules_see_every_request() {
        let new = || {
            let mut m = ConnectionManager::new(github_rules(), HashMap::new());
            m.connection_hosts.insert(1, "api.github.com".into());
            m.connection_hosts.insert(2, "www.github.com".into());
            m
        };
        for writes in [
            // A leading empty line, which servers skip.
            &[&b"\r\nDELETE /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"[..]][..],
            &[b"\nGET /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            // A second request after an empty line, in the same write.
            &[b"GET /ok HTTP/1.1\r\nHost: www.github.com\r\n\r\n\r\nDELETE /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            // Methods outside the common ones.
            &[b"PROPFIND /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            &[b"PURGE /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            &[b"X-CUSTOM", b" /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            // Almost-valid request lines that lenient servers accept.
            &[b"DELETE /other HTTP/1.1 \r\nHost: api.github.com\r\n\r\n"],
            &[b"DELETE\t/other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"],
            &[b"DELETE /other http/1.1\r\nHost: api.github.com\r\n\r\n"],
            // HTTP/0.9, which servers route by an absolute URL's host (or to
            // the default site, which may be the ruled host).
            &[b"GET http://api.github.com/other\r\n"],
            &[b"GET HTTP://api.github.com/other\r\n"],
            &[b"GET /admin\r\n"],
            &[b"GET\t/admin\r\n"],
            &[b"OPTIONS *\r\n"],
            // Non-HTTP bytes once a request has been framed.
            &[b"GET /ok HTTP/1.1\r\nHost: www.github.com\r\n\r\n", b"\x16\x03\x01"],
        ] {
            let mut m = new();
            let result: Result<Vec<_>, _> =
                writes.iter().map(|w| m.process_write_data(2, w)).collect();
            assert!(result.is_err(), "{writes:?}");
        }

        // A refused write fails every later write on the connection.
        let mut m = new();
        assert!(
            m.process_write_data(1, b"DELETE /x HTTP/1.1\r\n\r\n")
                .is_err()
        );
        assert!(
            m.process_write_data(1, b"GET /repos/org/repo HTTP/1.1\r\n\r\n")
                .is_err()
        );

        // A connection whose first bytes aren't HTTP to a host without rules
        // stays unparsed (the documented limitation).
        let mut m = new();
        assert_eq!(
            m.process_write_data(2, b"\x16\x03\x01").unwrap(),
            b"\x16\x03\x01"
        );
        let later = b"DELETE /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n";
        assert_eq!(m.process_write_data(2, later).unwrap(), later);
        // Text that isn't shaped like a request line counts as not HTTP too.
        for raw in [&b"SET k v\r\n"[..], b"GET key\r\n", b"PING\r\n"] {
            let mut m = new();
            assert_eq!(m.process_write_data(2, raw).unwrap(), raw);
        }

        // `://` in an origin-form target's query doesn't name a host.
        let mut m = new();
        for (handle, request) in [
            (
                2,
                &b"GET /login?next=https://example.com/ HTTP/1.1\r\nHost: www.github.com\r\n\r\n"[..],
            ),
            (
                1,
                b"GET /repos/org/repo?u=https://evil.com/ HTTP/1.1\r\nHost: api.github.com\r\n\r\n",
            ),
        ] {
            assert_eq!(m.process_write_data(handle, request).unwrap(), request);
        }

        // A rule can allow any method token.
        let config = NetConfig::permissive()
            .allow_http(HttpRule::new("dav.example.com").with_methods(["PROPFIND"]));
        let mut m = ConnectionManager::new(config, HashMap::new());
        m.connection_hosts.insert(1, "dav.example.com".into());
        let propfind = b"PROPFIND /files HTTP/1.1\r\nHost: dav.example.com\r\n\r\n";
        assert_eq!(m.process_write_data(1, propfind).unwrap(), propfind);
        assert!(
            m.process_write_data(1, b"MKCOL /files HTTP/1.1\r\nHost: dav.example.com\r\n\r\n")
                .is_err()
        );
    }

    /// A rule's paths must be ones a request can match.
    #[test]
    fn http_rule_paths_are_validated() {
        assert!(github_rules().validate().is_ok());
        for path in ["repos", "/a/../b", "/a//b", "/a?x", "/a%2fb", "/a%20b"] {
            let config = NetConfig::default().allow_http(HttpRule::new("h").with_paths([path]));
            assert!(config.validate().is_err(), "{path}");
        }
    }

    /// Requests are checked against the connected host and every host they
    /// name, and a refused request (or non-HTTP bytes to a ruled host) is
    /// never sent.
    #[test]
    fn http_rules_are_enforced_on_requests() {
        let mut m = ConnectionManager::new(github_rules(), HashMap::new());
        m.connection_hosts.insert(1, "api.github.com".into());
        m.connection_hosts.insert(2, "www.github.com".into());

        let ok = b"GET /repos/org/repo HTTP/1.1\r\nHost: api.github.com\r\n\r\n";
        assert_eq!(frame(&mut m, &[ok]), String::from_utf8_lossy(ok));
        for (handle, request) in [
            (1, &b"DELETE /repos/org/repo HTTP/1.1\r\nHost: api.github.com\r\n\r\n"[..]),
            (1, b"GET /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"),
            // Another host on the same server, naming the ruled one.
            (2, b"GET /other HTTP/1.1\r\nHost: api.github.com\r\n\r\n"),
            (2, b"GET https://api.github.com/other HTTP/1.1\r\nHost: www.github.com\r\n\r\n"),
            // A second request hidden in the first's body by a wrong length.
            (1, b"POST /repos/org/repo HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            (1, b"\x16\x03\x01\x00"),
            (1, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
        ] {
            assert!(m.process_write_data(handle, request).is_err(), "{request:?}");
            m.http_states.clear();
        }
        // Hosts named by a request are compared ignoring case, port, a
        // trailing dot and brackets, and must agree.
        for (handle, request) in [
            (2, &b"GET /other HTTP/1.1\r\nHost: API.GitHub.com.:443\r\n\r\n"[..]),
            (1, b"GET /repos/org/repo HTTP/1.1\r\nHost: api.github.com\r\nHost: api.github.com\r\n\r\n"),
            (2, b"GET https://www.github.com/ HTTP/1.1\r\nHost: other.github.com\r\n\r\n"),
        ] {
            assert!(m.process_write_data(handle, request).is_err(), "{request:?}");
            m.http_states.clear();
        }
        let absolute = b"GET https://api.github.com:443/repos/org/repo HTTP/1.1\r\nHost: API.github.com\r\n\r\n";
        assert!(m.process_write_data(1, absolute).is_ok());
        m.http_states.clear();

        // A pipelined request is checked too, and nothing is sent.
        let pipelined = b"POST /repos/org/repo HTTP/1.1\r\nContent-Length: 2\r\n\r\nhiDELETE /repos/org/repo HTTP/1.1\r\n\r\n";
        assert!(m.process_write_data(1, pipelined).is_err());
        m.http_states.clear();
        // Non-HTTP bytes still go through to hosts without rules.
        assert_eq!(
            m.process_write_data(2, b"\x16\x03\x01").unwrap(),
            b"\x16\x03\x01"
        );
    }

    /// Headers are buffered only up to a cap, and bytes go out as they are
    /// once their first line shows they aren't a request (or a byte no
    /// request starts with does), instead of waiting for a blank line.
    #[test]
    fn header_buffer_is_capped() {
        let mut m = framing_manager();
        let line = b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n";
        assert!(
            m.process_write_data(1, b"GET / HTTP/1.1\r\n")
                .unwrap()
                .is_empty()
        );
        let result = (0..MAX_HEADER_LEN / line.len() + 1)
            .map(|_| m.process_write_data(1, line))
            .find(Result::is_err);
        assert!(result.is_some(), "headers were buffered past the cap");

        let mut m = framing_manager();
        assert_eq!(frame(&mut m, &[b"G", b"E"]), "");
        assert_eq!(frame(&mut m, &[b"x hello PH"]), "");
        assert_eq!(frame(&mut m, &[b"\r\n"]), "GEx hello PH\r\n");
        assert_eq!(frame(&mut m, &[b"\x16\x03\x01"]), "\x16\x03\x01");
        let mut m = framing_manager();
        assert_eq!(frame(&mut m, &[b"\x16\x03\x01"]), "\x16\x03\x01");
    }

    /// The guest reads the placeholder where the server sent a real secret
    /// value, even when the value arrives split across reads.
    #[tokio::test]
    async fn reads_replace_secret_values_with_placeholders() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m = framing_manager();
        let (handle, server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let handle = handle.unwrap();
        let mut server = server;

        m.tcp_write(handle, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 0)
            .await
            .unwrap();
        // The value is split across chunks, and across writes.
        server
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\na re\r\n")
            .await
            .unwrap();
        server.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        server.write_all(b"9\r\nal b real\r\n").await.unwrap();
        // A possible start of a secret that nothing completes is still
        // delivered.
        server.write_all(b"3\r\n re\r\n0\r\n\r\n").await.unwrap();
        server.flush().await.unwrap();
        let mut got = Vec::new();
        while !got.ends_with(b"0\r\n\r\n") {
            let chunk = m.tcp_read(handle, 7, 0).await.unwrap();
            assert!(!chunk.is_empty(), "unexpected EOF");
            got.extend(chunk);
        }
        let (head, body) = got.split_at(got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4);
        assert_eq!(
            head,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
        );
        assert_eq!(dechunk(body), b"a PH b PH re");
    }

    /// The body a client gets from a chunked message: the chunks' data.
    fn dechunk(mut wire: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        loop {
            let line = wire.windows(2).position(|w| w == b"\r\n").unwrap();
            let size =
                usize::from_str_radix(std::str::from_utf8(&wire[..line]).unwrap(), 16).unwrap();
            wire = &wire[line + 2..];
            if size == 0 {
                return body;
            }
            body.extend_from_slice(&wire[..size]);
            wire = &wire[size + 2..];
        }
    }

    /// A framing manager connected to a local server on handle `handle`,
    /// with the server's side of the connection.
    async fn connected() -> (ConnectionManager, u32, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m = framing_manager();
        let (handle, server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        (m, handle.unwrap(), server)
    }

    /// Requests recorded by `tcp_write` frame their responses: a refused
    /// write doesn't forget the requests sent before it, and a response
    /// to no recorded request (unsolicited, or after writes that weren't
    /// requests) fails the connection rather than going through unframed.
    #[tokio::test]
    async fn responses_are_framed_by_the_requests_written() {
        let (mut m, handle, mut server) = connected().await;
        m.tcp_write(handle, b"HEAD / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 0)
            .await
            .unwrap();
        assert!(
            m.tcp_write(handle, b"\r\n\r\nGET / HTTP/1.1\r\n\r\n", 0)
                .await
                .is_err()
        );
        server
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nX-Echo: real\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            m.tcp_read(handle, 1024, 0).await.unwrap(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nX-Echo: PH\r\n\r\n"
        );

        // Unsolicited, on an idle keep-alive connection (a 408 before the
        // server closes it): the stream ends cleanly, and stays ended.
        let (mut m, handle, mut server) = connected().await;
        m.tcp_write(handle, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 0)
            .await
            .unwrap();
        server
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\nHTTP/1.1 408 Request Timeout\r\n\r\nre")
            .await
            .unwrap();
        let mut got = Vec::new();
        loop {
            let data = m.tcp_read(handle, 1024, 0).await.unwrap();
            if data.is_empty() {
                break;
            }
            got.extend(data);
        }
        assert_eq!(got, b"HTTP/1.1 204 No Content\r\n\r\n");
        assert!(m.tcp_read(handle, 64, 0).await.unwrap().is_empty());
    }

    /// Everything the server sends on `handle` until it closes.
    async fn read_to_end(m: &mut ConnectionManager, handle: u32) -> Vec<u8> {
        let mut got = Vec::new();
        loop {
            let data = m.tcp_read(handle, 1024, 0).await.unwrap();
            if data.is_empty() {
                return got;
            }
            got.extend(data);
        }
    }

    /// A connection that's another protocol from the start (its first write
    /// isn't a request, or the server speaks first) never has anything
    /// substituted, so its replies get best-effort masking (held back until
    /// a quiet spell) instead of failing: a one-byte reply comes through,
    /// an echoed real value is still masked, and a request-shaped write
    /// later isn't substituted.
    #[tokio::test]
    async fn other_protocols_from_the_start_pass_masked() {
        let (mut m, handle, mut server) = connected().await;
        // Postgres's SSLRequest, and its one-byte answer.
        m.tcp_write(handle, b"\x00\x00\x00\x08\x04\xd2\x16\x2f", 0)
            .await
            .unwrap();
        server.write_all(b"S").await.unwrap();
        assert_eq!(m.tcp_read(handle, 64, 0).await.unwrap(), b"S");
        let later = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: PH\r\n\r\n";
        m.tcp_write(handle, later, 0).await.unwrap();
        let mut received = vec![0; 8 + later.len()];
        server.read_exact(&mut received).await.unwrap();
        assert!(
            received.ends_with(later),
            "{:?}",
            String::from_utf8_lossy(&received)
        );
        server.write_all(b"+echo real").await.unwrap();
        drop(server);
        assert_eq!(read_to_end(&mut m, handle).await, b"+echo PH");

        // The server speaks first (e.g. an SMTP greeting).
        let (mut m, handle, mut server) = connected().await;
        server.write_all(b"220 ready real").await.unwrap();
        let mut got = Vec::new();
        while got.len() < b"220 ready PH".len() {
            got.extend(m.tcp_read(handle, 64, 0).await.unwrap());
        }
        assert_eq!(got, b"220 ready PH");
        m.tcp_write(handle, later, 0).await.unwrap();
        let mut received = vec![0; later.len()];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(&received[..], &later[..]);
    }

    /// A response head with bare-LF line endings is read as clients read
    /// it, and masked; malformed framing fails the connection.
    #[tokio::test]
    async fn bare_lf_heads_are_framed_and_bad_framing_fails() {
        let (mut m, handle, mut server) = connected().await;
        m.tcp_write(handle, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 0)
            .await
            .unwrap();
        server
            .write_all(b"HTTP/1.1 200 OK\nX-Echo: real\nContent-Length: 6\n\nreal!!")
            .await
            .unwrap();
        let mut got = Vec::new();
        while !got.ends_with(b"PH!!") {
            got.extend(m.tcp_read(handle, 1024, 0).await.unwrap());
        }
        assert_eq!(
            got,
            b"HTTP/1.1 200 OK\nX-Echo: PH\nContent-Length: 6\n\nPH!!"
        );

        for response in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nre"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nre",
            b"HTTP/1.1 2000 OK\r\n\r\n",
        ] {
            let (mut m, handle, mut server) = connected().await;
            m.tcp_write(handle, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 0)
                .await
                .unwrap();
            server.write_all(response).await.unwrap();
            let mut result = m.tcp_read(handle, 1024, 0).await;
            // The head may come first; the connection fails before the body.
            if result.as_ref().is_ok_and(|data| !data.is_empty()) {
                result = m.tcp_read(handle, 1024, 0).await;
            }
            assert!(result.is_err(), "{:?}", String::from_utf8_lossy(response));
        }
    }

    /// One read, and whether it waited out the grace period first.
    async fn timed_read(
        stream: &mut tokio::io::DuplexStream,
        state: &mut ReadState,
        len: u32,
    ) -> (Vec<u8>, bool) {
        let start = tokio::time::Instant::now();
        let data = read_masked(stream, state, &mut sent([]), len, Duration::from_secs(30))
            .await
            .unwrap();
        (data, start.elapsed() >= CARRY_GRACE)
    }

    fn masks(values: &[&str]) -> Vec<(Vec<u8>, Vec<u8>)> {
        values
            .iter()
            .map(|v| (v.as_bytes().to_vec(), b"PH".to_vec()))
            .collect()
    }

    /// Request framing with `requests` sent and awaiting responses.
    fn sent(requests: impl IntoIterator<Item = RequestKind>) -> HttpParsingState {
        let requests: std::collections::VecDeque<_> = requests.into_iter().collect();
        HttpParsingState {
            started: !requests.is_empty(),
            requests,
            ..HttpParsingState::default()
        }
    }

    /// A read state past a `101`, where bytes aren't framed HTTP and held
    /// ones go out after the grace period.
    fn upgraded(values: &[&str]) -> ReadState {
        let mut state = ReadState::new(masks(values));
        let masker = state.masker.as_mut().unwrap();
        masker.feed(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\n",
            &mut std::collections::VecDeque::from([RequestKind::Other]),
        );
        masker.out.clear();
        state
    }

    /// The server sends `probe` in one write; the length of each read the
    /// guest gets and whether it was delayed, until it has every byte.
    async fn read_pattern(values: &[&str], probe: &[u8], len: u32) -> Vec<(usize, bool)> {
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = upgraded(values);
        server.write_all(probe).await.unwrap();
        let mut pattern = Vec::new();
        let mut total = 0;
        while total < probe.len() {
            let (data, delayed) = timed_read(&mut guest, &mut state, len).await;
            assert!(!data.is_empty());
            total += data.len();
            pattern.push((data.len(), delayed));
        }
        pattern
    }

    /// An echoing server sends back bytes the guest chose. Whether they're a
    /// prefix of a real secret value must not change how they're delivered:
    /// read lengths and delays are the same for matching and non-matching
    /// probes of the same length, with two secrets of different lengths on
    /// the host. Holding back only possible prefixes made these differ.
    #[tokio::test(start_paused = true)]
    async fn masking_hold_back_is_not_a_prefix_oracle() {
        let long = "SECRETVALUE";
        let short = "abc";
        let filler = b"qrstuvwxyz";
        for (secret, k) in [(long, 2), (long, 7), (short, 2)] {
            for trial in 0..50_usize {
                let pad = vec![b'-'; 12 + trial % 7];
                let len = u32::try_from(1 + trial % 40).unwrap();
                let matching = [&pad[..], &secret.as_bytes()[..k]].concat();
                let other: Vec<u8> = (0..k).map(|i| filler[(trial + i) % filler.len()]).collect();
                let non_matching = [&pad[..], &other[..]].concat();
                let a = read_pattern(&[long, short], &matching, len).await;
                let b = read_pattern(&[long, short], &non_matching, len).await;
                assert_eq!(a, b, "{secret}[..{k}], trial {trial}");
            }
        }
    }

    /// Both secrets are masked when they arrive one byte per write and the
    /// guest reads one byte at a time.
    #[tokio::test(start_paused = true)]
    async fn masking_handles_byte_by_byte_values() {
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = upgraded(&["SECRETVALUE", "abc"]);
        assert_eq!(state.masker.as_ref().unwrap().hold(), 10);
        let written = b"x SECRETVALUE y abc z";
        let mut got = Vec::new();
        for &byte in written {
            server.write_all(&[byte]).await.unwrap();
            if let Ok(data) = tokio::time::timeout(
                Duration::from_millis(1),
                read_masked(
                    &mut guest,
                    &mut state,
                    &mut sent([]),
                    1,
                    Duration::from_secs(30),
                ),
            )
            .await
            {
                got.extend(data.unwrap());
            }
        }
        drop(server);
        loop {
            let (data, _) = timed_read(&mut guest, &mut state, 1).await;
            if data.is_empty() {
                break;
            }
            got.extend(data);
        }
        assert_eq!(got, b"x PH y PH z");
    }

    /// The held-back tail goes out at once when the stream ends, and after
    /// the grace period when it goes quiet.
    #[tokio::test(start_paused = true)]
    async fn masking_releases_tail_on_eof_and_quiet() {
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = upgraded(&["SECRETVALUE"]);
        server.write_all(b"hello").await.unwrap();
        assert_eq!(
            timed_read(&mut guest, &mut state, 64).await,
            (b"hello".to_vec(), true)
        );
        server.write_all(b"bye").await.unwrap();
        drop(server);
        assert_eq!(
            timed_read(&mut guest, &mut state, 64).await,
            (b"bye".to_vec(), false)
        );
    }

    /// Only secrets allowed for a connection's host are masked on it, so
    /// other hosts' reads are neither masked nor held back.
    #[tokio::test(start_paused = true)]
    async fn masking_only_applies_to_secret_hosts() {
        let m = framing_manager();
        let host = |h: &str| read_masks(&m.config, &m.secrets, Some(&h.to_string()), true);
        assert_eq!(host("api.example.com"), masks(&["real"]));
        assert!(host("echo.example.net").is_empty());
        assert!(read_masks(&m.config, &m.secrets, None, true).is_empty());
        // Nor on a connection whose requests don't get values substituted.
        assert!(
            read_masks(
                &m.config,
                &m.secrets,
                Some(&"api.example.com".into()),
                false
            )
            .is_empty()
        );

        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = ReadState::new(host("echo.example.net"));
        server.write_all(b"a real").await.unwrap();
        assert_eq!(
            timed_read(&mut guest, &mut state, 64).await,
            (b"a real".to_vec(), false)
        );
    }

    /// Reads until the stream ends or a read fails (after `io_timeout`),
    /// returning each read's length and whether it waited out the grace
    /// period, and everything read.
    async fn read_all(
        stream: &mut (impl tokio::io::AsyncRead + Unpin),
        state: &mut ReadState,
        requests: &mut HttpParsingState,
        io_timeout: Duration,
    ) -> (Vec<(usize, bool)>, Vec<u8>) {
        let (mut pattern, mut got) = (Vec::new(), Vec::new());
        loop {
            let start = tokio::time::Instant::now();
            match read_masked(stream, state, requests, 7, io_timeout).await {
                Ok(data) if !data.is_empty() => {
                    pattern.push((data.len(), start.elapsed() >= CARRY_GRACE));
                    got.extend(data);
                }
                _ => return (pattern, got),
            }
        }
    }

    /// A server that splits a secret value inside a framed response and
    /// pauses (as an echo of the guest's own bytes can be made to) doesn't
    /// get the first part past the masking: in a header, a `Content-Length`
    /// body and a chunk's data, held bytes wait for the rest, however long.
    #[tokio::test(start_paused = true)]
    async fn framed_responses_stay_masked_across_pauses() {
        for (before, after) in [
            (
                &b"HTTP/1.1 200 OK\r\nX-Echo: SECRETVA"[..],
                &b"LUE\r\nContent-Length: 0\r\n\r\n"[..],
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\nx SECRETVA",
                b"LUE y",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nf\r\nx SECRETVA",
                b"LUE y\r\n0\r\n\r\n",
            ),
        ] {
            let (mut guest, mut server) = tokio::io::duplex(4096);
            let mut state = ReadState::new(masks(&["SECRETVALUE"]));
            let mut requests = sent([RequestKind::Other]);
            let send = async {
                server.write_all(before).await.unwrap();
                tokio::time::sleep(Duration::from_secs(20)).await;
                server.write_all(after).await.unwrap();
                drop(server);
            };
            let ((_, got), ()) = tokio::join!(
                read_all(
                    &mut guest,
                    &mut state,
                    &mut requests,
                    Duration::from_secs(30)
                ),
                send
            );
            let got = String::from_utf8(got).unwrap();
            assert!(got.contains("PH") && !got.contains("SECRET"), "{got:?}");
        }
    }

    /// Whether an echoed probe is a prefix of a real value doesn't change
    /// how a framed response is delivered either: in a `Content-Length`
    /// body and in a chunk followed by a quiet spell, read lengths and
    /// delays match those of a non-matching probe of the same length.
    #[tokio::test(start_paused = true)]
    async fn framed_hold_back_is_not_a_prefix_oracle() {
        let long = "SECRETVALUE";
        let filler = b"qrstuvwxyz";
        let pattern = |wire: Vec<u8>| async move {
            let (mut guest, mut server) = tokio::io::duplex(4096);
            let mut state = ReadState::new(masks(&[long, "abc"]));
            let mut requests = sent([RequestKind::Other]);
            server.write_all(&wire).await.unwrap();
            read_all(
                &mut guest,
                &mut state,
                &mut requests,
                Duration::from_secs(1),
            )
            .await
            .0
        };
        for k in [2, 7, 10] {
            for trial in 0..50_usize {
                let pad = vec![b'-'; 12 + trial % 7];
                let matching = [&pad[..], &long.as_bytes()[..k]].concat();
                let other: Vec<u8> = (0..k).map(|i| filler[(trial + i) % filler.len()]).collect();
                let non_matching = [&pad[..], &other[..]].concat();
                for body in [&matching, &non_matching] {
                    assert_eq!(body.len(), matching.len());
                }
                let length = |body: &[u8]| {
                    [
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                            .as_bytes(),
                        body,
                    ]
                    .concat()
                };
                let chunk = |body: &[u8]| {
                    [
                        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
                        format!("{:x}\r\n", body.len()).as_bytes(),
                        body,
                        b"\r\n",
                    ]
                    .concat()
                };
                assert_eq!(
                    pattern(length(&matching)).await,
                    pattern(length(&non_matching)).await,
                    "length, [..{k}], trial {trial}"
                );
                assert_eq!(
                    pattern(chunk(&matching)).await,
                    pattern(chunk(&non_matching)).await,
                    "chunked, [..{k}], trial {trial}"
                );
            }
        }
    }

    /// A stream that returns each scripted result once, then ends.
    struct Scripted(std::collections::VecDeque<std::io::Result<Vec<u8>>>);

    impl tokio::io::AsyncRead for Scripted {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(match self.0.pop_front() {
                Some(Ok(data)) => {
                    buf.put_slice(&data);
                    Ok(())
                }
                Some(Err(e)) => Err(e),
                None => Ok(()),
            })
        }
    }

    /// Bytes held back when a read fails are delivered first, and the error
    /// on the next read, rather than lost.
    #[tokio::test(start_paused = true)]
    async fn a_read_error_delivers_held_bytes_first() {
        let mut stream = Scripted(
            [
                Ok(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nhello".to_vec()),
                Err(std::io::ErrorKind::ConnectionReset.into()),
            ]
            .into(),
        );
        let mut state = ReadState::new(masks(&["SECRETVALUE"]));
        let mut requests = sent([RequestKind::Other]);
        let mut got = Vec::new();
        let err = loop {
            match read_masked(
                &mut stream,
                &mut state,
                &mut requests,
                64,
                Duration::from_secs(30),
            )
            .await
            {
                Ok(data) => {
                    assert!(!data.is_empty());
                    got.extend(data);
                }
                Err(e) => break e,
            }
        };
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
        assert!(
            got.ends_with(b"hello"),
            "{:?}",
            String::from_utf8_lossy(&got)
        );
    }

    /// Through the manager, a guest's `timeout_ms = 1` doesn't shorten the
    /// grace period either. (Real time: paused time would advance past real
    /// I/O.)
    #[tokio::test]
    async fn guest_timeout_ms_doesnt_shorten_the_grace() {
        let (mut m, handle, mut server) = connected().await;
        m.tcp_write(
            handle,
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: x\r\n\r\n",
            0,
        )
        .await
        .unwrap();
        server
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\nre")
            .await
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.extend(m.tcp_read(handle, 64, 0).await.unwrap());
        }
        let start = tokio::time::Instant::now();
        assert_eq!(m.tcp_read(handle, 64, 1).await.unwrap(), b"re");
        assert!(start.elapsed() >= CARRY_GRACE);
    }

    /// One read's result and how long it took.
    async fn timed(
        guest: &mut tokio::io::DuplexStream,
        state: &mut ReadState,
        requests: &mut HttpParsingState,
        timeout: Duration,
    ) -> (Result<Vec<u8>, std::io::ErrorKind>, Duration) {
        let start = tokio::time::Instant::now();
        let result = read_masked(guest, state, requests, 64, timeout).await;
        (result.map_err(|e| e.kind()), start.elapsed())
    }

    /// A guest reading with `timeout_ms = 1` can't make the grace period
    /// release half a value either: bytes after a `101` split mid-value,
    /// with a pause shorter than the grace, stay masked however short each
    /// read's timeout.
    #[tokio::test(start_paused = true)]
    async fn a_short_guest_timeout_doesnt_split_a_value() {
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = upgraded(&["SECRETVALUE"]);
        let mut requests = sent([]);
        let send = async {
            server.write_all(b"x SECRETVA").await.unwrap();
            tokio::time::sleep(CARRY_GRACE / 2).await;
            server.write_all(b"LUE y").await.unwrap();
            drop(server);
        };
        let read = async {
            let mut got = Vec::new();
            loop {
                match read_masked(
                    &mut guest,
                    &mut state,
                    &mut requests,
                    64,
                    Duration::from_millis(1),
                )
                .await
                {
                    Ok(data) if data.is_empty() => return got,
                    Ok(data) => got.extend(data),
                    Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
                }
            }
        };
        let (got, ()) = tokio::join!(read, send);
        assert_eq!(got, b"x PH y");
    }

    /// Held bytes after a `101` go out after the full grace period, however
    /// short the read timeout (here 1 ms, as a guest's `timeout_ms` can ask);
    /// held bytes in a framed body, a close-delimited one included, wait
    /// for more of it or its end, and the read times out at its own timeout
    /// instead.
    #[tokio::test(start_paused = true)]
    async fn quiet_release_waits_the_full_grace() {
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = upgraded(&["SECRETVALUE"]);
        let mut requests = sent([]);
        server.write_all(b"hello").await.unwrap();
        for timeout in [Duration::from_secs(30), Duration::from_millis(1)] {
            let (body, waited) = timed(&mut guest, &mut state, &mut requests, timeout).await;
            assert_eq!(
                (body.unwrap(), waited),
                (b"hello".to_vec(), CARRY_GRACE),
                "{timeout:?}"
            );
            server.write_all(b"hello").await.unwrap();
        }

        // A close-delimited body's tail waits for the close.
        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = ReadState::new(masks(&["SECRETVALUE"]));
        let mut requests = sent([RequestKind::Other]);
        server
            .write_all(b"HTTP/1.0 200 OK\r\n\r\nhello")
            .await
            .unwrap();
        let (head, _) = timed(
            &mut guest,
            &mut state,
            &mut requests,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(head.unwrap(), b"HTTP/1.0 200 OK\r\n\r\n");
        let (err, waited) = timed(
            &mut guest,
            &mut state,
            &mut requests,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(
            (err.unwrap_err(), waited),
            (std::io::ErrorKind::TimedOut, Duration::from_secs(30))
        );
        drop(server);
        let (body, _) = timed(
            &mut guest,
            &mut state,
            &mut requests,
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(body.unwrap(), b"hello");

        let (mut guest, mut server) = tokio::io::duplex(4096);
        let mut state = ReadState::new(masks(&["SECRETVALUE"]));
        let mut requests = sent([RequestKind::Other]);
        server
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nhello")
            .await
            .unwrap();
        let (head, _) = timed(
            &mut guest,
            &mut state,
            &mut requests,
            Duration::from_secs(30),
        )
        .await;
        assert!(head.unwrap().ends_with(b"\r\n\r\n"));
        let (err, waited) = timed(
            &mut guest,
            &mut state,
            &mut requests,
            Duration::from_millis(30),
        )
        .await;
        assert_eq!(
            (err.unwrap_err(), waited),
            (std::io::ErrorKind::TimedOut, Duration::from_millis(30))
        );
    }
}
