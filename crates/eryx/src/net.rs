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

mod credential;
mod policy;
mod proxy;
mod response;
pub use credential::Credential;
use ipnet::{Ipv4Net, Ipv6Net};
use policy::{CheckedTcp, NetPolicy, RequestFramer, ResponseMasker};
pub use proxy::{ChildNetPolicy, ChildNetwork, ChildProxy};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
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

    /// Set the maximum number of concurrent connections, the execution's and
    /// its child processes' together (see
    /// [`ConnectionManager::child_network`]). A child past it gets a `503`.
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
    /// every connection, its child processes' included (see
    /// [`ConnectionManager::child_network`]). Past it, reads and writes fail
    /// with [`TcpError::NotPermitted`]; a child's request gets a `403`, or its
    /// connection is closed if a response had started. Bytes are counted as
    /// the code wrote and read them, without TLS overhead, except through a
    /// child's blind tunnel (see [`ChildProxy::start`]), which only sees the
    /// encrypted bytes and so also counts the handshake and record overhead.
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
        if let Some(target) = request_target(target)
            && rules.any(|r| r.allows(method, &target))
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

    /// Fail if a secret has no host it could be sent to (see
    /// [`NetConfig::secret_hosts`]), or one of its
    /// [HTTP rules](crate::SecretConfig::with_http_rules) can't match a
    /// request.
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
        let mut secrets: Vec<_> = secrets.iter().collect();
        secrets.sort_by_key(|(name, _)| *name);
        for (name, secret) in secrets {
            if self.secret_hosts(secret).is_empty() {
                return Err(format!(
                    "secret '{name}' has no allowed hosts: give it allowed_hosts, or list the \
                     hosts in NetConfig.allowed_hosts with allow_all_hosts off"
                ));
            }
            for rule in &secret.http_rules {
                rule.validate()
                    .map_err(|e| format!("secret '{name}': {e}"))?;
            }
        }
        Ok(())
    }

    /// Fail if a setting can't work: a zero connect or I/O timeout would
    /// make every connection time out immediately, an HTTP rule's path
    /// must be one a request can match, and its query patterns must compile.
    ///
    /// [`SandboxBuilder::build`](crate::SandboxBuilder::build) runs this
    /// check when networking is enabled, and every network policy (e.g.
    /// [`ConnectionManager::new`]) refuses a config that fails it.
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
/// - Queries are anchored regular expressions ([`regex`] syntax) matched
///   against the whole raw query string: the text after `?`, as sent, not
///   decoded, and empty when there's none. `service=git-upload-pack` matches
///   exactly that query, `(service=git-upload-pack)?` also no query, and
///   neither `service=git-upload-pack&service=git-receive-pack` nor
///   `service=git%2Dupload-pack`. No queries means any query.
/// - Paths are matched without the query, with percent-escapes decoded. A
///   path with a `.` or `..` segment (escaped or not, or
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
///
/// // Git fetches, but not pushes.
/// let git = NetConfig::default().allow_host("git.example.com").allow_http(
///     HttpRule::new("git.example.com")
///         .with_methods(["GET"])
///         .with_paths(["/org/repo.git/info/refs"])
///         .with_queries(["service=git-upload-pack"]),
/// );
/// let check = |target| git.check_http_request("git.example.com", "GET", target);
/// assert!(check("/org/repo.git/info/refs?service=git-upload-pack").is_ok());
/// assert!(check("/org/repo.git/info/refs?service=git-receive-pack").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRule {
    host: String,
    methods: Vec<String>,
    paths: Vec<String>,
    queries: QueryPatterns,
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
            queries: QueryPatterns::default(),
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

    /// Only allow requests whose raw query matches one of these anchored
    /// regular expressions; see [`HttpRule`]. A pattern that doesn't compile
    /// fails [`NetConfig::validate`].
    #[must_use]
    pub fn with_queries(mut self, queries: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.queries = QueryPatterns::new(queries.into_iter().map(Into::into).collect());
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

    /// The allowed query patterns; empty allows any query.
    #[must_use]
    pub fn queries(&self) -> &[String] {
        self.queries.patterns()
    }

    fn validate(&self) -> Result<(), String> {
        for path in &self.paths {
            if !is_normalized_path(path) {
                return Err(format!(
                    "HTTP rule path {path:?} for {} must be a normalized absolute path",
                    self.host
                ));
            }
        }
        self.queries
            .validate()
            .map_err(|e| format!("HTTP rule for {}: {e}", self.host))
    }

    /// Whether this rule allows `method` on `target`.
    fn allows(&self, method: &str, target: &RequestTarget) -> bool {
        let method_ok =
            self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method));
        let path_ok =
            self.paths.is_empty() || self.paths.iter().any(|p| path_is_under(&target.path, p));
        method_ok && path_ok && self.queries.matches(&target.query)
    }
}

/// Regular expressions each matched against a whole raw query string; see
/// [`HttpRule::with_queries`]. No patterns matches any query.
#[derive(Clone, Debug)]
pub(crate) struct QueryPatterns {
    patterns: Vec<String>,
    /// The patterns anchored and compiled, or why one didn't compile.
    compiled: Result<Vec<regex::Regex>, String>,
}

impl QueryPatterns {
    pub(crate) fn new(patterns: Vec<String>) -> Self {
        let compiled = patterns
            .iter()
            .map(|p| {
                // Compiled alone first: wrapping a pattern like `a)|(b`
                // would compile, unanchored.
                regex::Regex::new(p)
                    .and_then(|_| regex::Regex::new(&format!(r"\A(?:{p})\z")))
                    .map_err(|e| {
                        format!("query pattern {p:?} is not a valid regular expression: {e}")
                    })
            })
            .collect();
        Self { patterns, compiled }
    }

    pub(crate) fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Fail if a pattern doesn't compile.
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.compiled.as_ref().map(|_| ()).map_err(Clone::clone)
    }

    /// Whether the raw `query` (empty if there's none) matches. Never, if a
    /// pattern doesn't compile.
    pub(crate) fn matches(&self, query: &str) -> bool {
        self.patterns.is_empty()
            || self
                .compiled
                .as_ref()
                .is_ok_and(|regexes| regexes.iter().any(|re| re.is_match(query)))
    }
}

impl Default for QueryPatterns {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// Equal when the patterns are: the compiled form follows from them.
impl PartialEq for QueryPatterns {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for QueryPatterns {}

/// Whether a request to `host` matches one of `rules`, host included: the
/// requests a secret's [HTTP rules](crate::SecretConfig::with_http_rules)
/// allow it in. No rules matches any request.
pub(crate) fn request_matches_rules(
    rules: &[HttpRule],
    host: &str,
    method: &str,
    target: &str,
) -> bool {
    if rules.is_empty() {
        return true;
    }
    let host = normalize_host(host);
    request_target(target).is_some_and(|target| {
        rules.iter().any(|r| {
            host_matches_pattern(&host, &normalize_host(&r.host)) && r.allows(method, &target)
        })
    })
}

/// Whether the decoded `path` is `prefix` or under it: `/a` covers `/a` and
/// `/a/b` but not `/ab`; `/a/` covers everything below `/a/`.
fn path_is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/'))
}

/// Whether `path` is a path prefix [`request_path`] leaves as it is: a
/// normalized absolute path with no query or fragment.
fn is_normalized_path(path: &str) -> bool {
    request_path(path).as_deref() == Some(path) && !path.contains(['?', '#'])
}

/// The decoded path of request target `target`; see [`request_target`].
fn request_path(target: &str) -> Option<String> {
    request_target(target).map(|t| t.path)
}

/// What [`HttpRule`]s match in a request target.
#[derive(Debug, PartialEq, Eq)]
struct RequestTarget {
    /// The percent-decoded path, without the query.
    path: String,
    /// The raw query, as sent: the text after `?`, or empty without one.
    query: String,
}

/// The path and query of request target `target` (origin-form, a path, or
/// absolute-form, an absolute URL); `None` if it has no path or its path is
/// ambiguous: see [`HttpRule`]. Any other target (`*`, an authority, or
/// something a server might resolve as a relative path, like
/// `../x?u=http://host/`) has none.
fn request_target(target: &str) -> Option<RequestTarget> {
    let uri = if target.starts_with('/') {
        parse_target(target)?
    } else {
        absolute_form(target)?
    };
    // The raw path and query: parse_target checked the parsed target is
    // the text.
    let path = uri.path();
    let query = uri.query().unwrap_or_default().to_owned();

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
    Some(RequestTarget {
        path: decoded,
        query,
    })
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

/// What one execution may use of the network, shared by its guest's
/// [`ConnectionManager`] and its children's proxies: upstream connections
/// held at once ([`NetConfig::max_connections`], unlimited for 0) and bytes
/// moved ([`NetConfig::max_bytes`]).
#[derive(Clone, Debug)]
pub(crate) struct NetBudget {
    connections: Option<Arc<tokio::sync::Semaphore>>,
    bytes: Arc<ByteMeter>,
}

impl NetBudget {
    pub(crate) fn new(max_connections: u32, max_bytes: Option<u64>) -> Self {
        Self {
            connections: (max_connections > 0)
                .then(|| Arc::new(tokio::sync::Semaphore::new(max_connections as usize))),
            bytes: Arc::new(ByteMeter {
                max: max_bytes,
                ..ByteMeter::default()
            }),
        }
    }

    /// A connection's share, to hold while it's open (`None` if connections
    /// are unlimited), or an error when they're all taken.
    pub(crate) fn try_acquire(
        &self,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, tokio::sync::TryAcquireError> {
        self.connections
            .as_ref()
            .map(|budget| Arc::clone(budget).try_acquire_owned())
            .transpose()
    }

    pub(crate) fn bytes(&self) -> &ByteMeter {
        &self.bytes
    }
}

/// Bytes moved against an optional limit, by the guest and its children's
/// proxies at once. A read is capped at what's left when it starts and
/// charged for what arrived, with anything past the limit dropped; a write
/// is charged in full before it starts, or refused. So nothing idle holds
/// any of the limit, and transfers racing on several connections never go
/// over it together.
#[derive(Debug, Default)]
pub(crate) struct ByteMeter {
    max: Option<u64>,
    /// Moved, or being written, counted against `max`.
    used: std::sync::atomic::AtomicU64,
    sent: std::sync::atomic::AtomicU64,
    received: std::sync::atomic::AtomicU64,
}

impl ByteMeter {
    pub(crate) fn max(&self) -> Option<u64> {
        self.max
    }

    /// What's left of the limit, if there is one.
    pub(crate) fn remaining(&self) -> Option<u64> {
        let used = self.used.load(std::sync::atomic::Ordering::Acquire);
        self.max.map(|max| max.saturating_sub(used))
    }

    /// Charge up to `n` bytes received: all of them, or what's left. Returns
    /// how many were charged, the ones that may be passed on.
    pub(crate) fn take_received(&self, n: u64) -> u64 {
        let granted = self.reserve(n, false);
        self.count(granted, false);
        granted
    }

    /// Charge all `n` bytes about to be sent, or none if they don't fit.
    pub(crate) fn take_sent(&self, n: u64) -> bool {
        let fits = self.reserve(n, true) == n;
        if fits {
            self.count(n, true);
        }
        fits
    }

    /// Give back `n` bytes charged by [`take_sent`](Self::take_sent) that
    /// weren't sent after all.
    pub(crate) fn refund_sent(&self, n: u64) {
        use std::sync::atomic::Ordering;
        if self.max.is_some() {
            self.used.fetch_sub(n, Ordering::AcqRel);
        }
        self.sent.fetch_sub(n, Ordering::Relaxed);
    }

    /// Add up to `want` to `used`: all of it or what fits (or, with `whole`,
    /// none unless all of it fits). Returns what was added.
    fn reserve(&self, want: u64, whole: bool) -> u64 {
        use std::sync::atomic::Ordering;
        let Some(max) = self.max else { return want };
        let mut granted = 0;
        let _ = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                let left = max.saturating_sub(used);
                granted = if whole && want > left {
                    0
                } else {
                    want.min(left)
                };
                Some(used + granted)
            });
        granted
    }

    fn count(&self, n: u64, sent: bool) {
        let counter = if sent { &self.sent } else { &self.received };
        counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn traffic(&self) -> NetTraffic {
        use std::sync::atomic::Ordering;
        NetTraffic {
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
        }
    }
}

/// Manages TCP and TLS connections for a sandbox instance.
///
/// Each sandbox has its own connection manager, which tracks active connections
/// and enforces the network policy.
#[derive(Debug)]
pub struct ConnectionManager {
    /// The network policy, shared with each connection's framer and masker.
    policy: Arc<NetPolicy>,
    /// The execution's connection budget, shared with its children's proxies.
    budget: NetBudget,
    /// Each open connection's share of `budget`, by handle.
    permits: HashMap<u32, tokio::sync::OwnedSemaphorePermit>,
    tcp_connections: HashMap<u32, CheckedTcp>,
    tls_connections: HashMap<u32, TlsStream<TcpStream>>,
    next_handle: u32,
    /// Request framing per connection handle, created on first write.
    http_states: HashMap<u32, RequestFramer>,
    /// Track which host each connection is connected to
    connection_hosts: HashMap<u32, String>,
    /// Response masking per connection handle, created on first read.
    read_states: HashMap<u32, ResponseMasker>,
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

impl ConnectionManager {
    /// Bytes sent and received so far, by the guest and by its children
    /// through proxies started from [`child_network`](Self::child_network).
    #[must_use]
    pub fn traffic(&self) -> NetTraffic {
        self.budget.bytes().traffic()
    }

    fn over_limit(&self) -> TcpError {
        TcpError::NotPermitted(format!(
            "network limit reached: {} bytes sent and received",
            self.budget.bytes().max().unwrap_or_default()
        ))
    }

    /// `Err` once the byte limit is spent.
    fn check_bytes_left(&self) -> Result<(), TcpError> {
        match self.budget.bytes().remaining() {
            Some(0) => Err(self.over_limit()),
            _ => Ok(()),
        }
    }

    /// Charge a write of all `len` bytes, or refuse it if they'd go over the
    /// limit.
    fn take_write(&self, len: usize) -> Result<(), TcpError> {
        if self.budget.bytes().take_sent(len as u64) {
            Ok(())
        } else {
            Err(self.over_limit())
        }
    }

    /// `len` capped at what's left of the limit; `Err` once none is.
    fn read_len(&self, len: u32) -> Result<u32, TcpError> {
        Ok(match self.budget.bytes().remaining() {
            Some(0) => return Err(self.over_limit()),
            Some(left) => len.min(u32::try_from(left).unwrap_or(u32::MAX)),
            None => len,
        })
    }

    /// Charge `data` as received, dropping what's past the limit (if others
    /// on the budget got there first).
    fn charge_read(&self, mut data: Vec<u8>) -> Vec<u8> {
        let granted = self.budget.bytes().take_received(data.len() as u64);
        data.truncate(usize::try_from(granted).unwrap_or(usize::MAX));
        data
    }

    /// Create a new connection manager with the given config and secrets.
    ///
    /// # Errors
    ///
    /// Fails, as [`NetConfig::validate`] does, if `config` is invalid.
    pub fn new(
        config: NetConfig,
        secrets: HashMap<String, crate::secrets::SecretConfig>,
    ) -> Result<Self, String> {
        let budget = NetBudget::new(config.max_connections, config.max_bytes);
        let policy = Arc::new(NetPolicy::new(config, secrets)?);
        Ok(Self {
            budget,
            permits: HashMap::new(),
            policy,
            tcp_connections: HashMap::new(),
            tls_connections: HashMap::new(),
            next_handle: 1,
            http_states: HashMap::new(),
            connection_hosts: HashMap::new(),
            read_states: HashMap::new(),
        })
    }

    /// The network for this execution's child processes, if `network` gives
    /// them one: this manager's policy (narrowed, for
    /// [`ChildNetwork::Narrow`]) and secrets, and its connection budget, so
    /// that the guest and its children together never hold more than
    /// [`NetConfig::max_connections`] connections or move more than
    /// [`NetConfig::max_bytes`] bytes.
    ///
    /// # Errors
    ///
    /// Fails if a [`ChildNetwork::Narrow`] config is invalid (see
    /// [`NetConfig::validate`]), as `"child network: …"`.
    pub fn child_network(&self, network: ChildNetwork) -> Result<Option<ChildNetPolicy>, String> {
        ChildNetPolicy::new(Arc::clone(&self.policy), self.budget.clone(), network)
    }

    /// Get the total number of active connections.
    #[cfg(test)]
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
        self.framer(handle)?.frame(data)
    }

    /// The request framer of `handle`, created on its first write or read.
    fn framer(&mut self, handle: u32) -> Result<&mut RequestFramer, TcpError> {
        if !self.http_states.contains_key(&handle) {
            let host = self
                .connection_hosts
                .get(&handle)
                .ok_or(TcpError::InvalidHandle)?;
            let framer = self
                .policy
                .request_framer(host, self.tls_connections.contains_key(&handle));
            self.http_states.insert(handle, framer);
        }
        self.http_states
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)
    }

    /// Create the response masker of `handle`, paired with its request
    /// framer, if it has none yet.
    fn ensure_masker(&mut self, handle: u32) -> Result<(), TcpError> {
        if !self.read_states.contains_key(&handle) {
            let masker = self.framer(handle)?.response_masker();
            self.read_states.insert(handle, masker);
        }
        Ok(())
    }

    /// Whether writes on `handle` go through HTTP request framing.
    fn frames_requests(&self, handle: u32) -> bool {
        self.policy
            .frames_requests(self.tls_connections.contains_key(&handle))
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
        // Refuse a disallowed host before the connection limit, as the
        // policy's own check in `connect` would.
        self.policy.check_host(host)?;
        // A spent byte limit takes no new connections, whose lookups and
        // handshakes it wouldn't count, as the proxy refuses them.
        self.check_bytes_left()?;

        let permit = self
            .budget
            .try_acquire()
            .map_err(|_| TcpError::NotPermitted("connection limit reached".into()))?;

        let connect_timeout =
            Self::effective_timeout(self.policy.config.connect_timeout, timeout_ms);
        let tcp = self.policy.connect(host, port, connect_timeout).await?;

        let handle = self.alloc_handle();
        self.tcp_connections.insert(handle, tcp);
        if let Some(permit) = permit {
            self.permits.insert(handle, permit);
        }
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
        let io_timeout = Self::effective_timeout(self.policy.config.io_timeout, timeout_ms);
        let len = self.read_len(len)?;
        self.ensure_masker(handle)?;
        let stream = self
            .tcp_connections
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?
            .stream();
        let masker = self
            .read_states
            .get_mut(&handle)
            .ok_or(TcpError::InvalidHandle)?;
        let data = masker
            .read(stream, len, io_timeout)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;
        Ok(self.charge_read(data))
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
        self.take_write(data.len())?;
        let (accepted, written) = self.write_framed_tcp(handle, data, timeout_ms).await;
        if written.is_err() {
            // A write that times out or fails may have sent some of it: only
            // what the stream never accepted is refunded.
            let unsent = data.len().saturating_sub(accepted);
            self.budget.bytes().refund_sent(unsent as u64);
        }
        written?;
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write `data`, substituting secrets and checking HTTP rules if the
    /// connection frames requests, returning how many bytes the stream
    /// accepted, finished or not.
    async fn write_framed_tcp(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> (usize, Result<(), TcpError>) {
        // Fast path: nothing to substitute or check = no parsing
        if !self.frames_requests(handle) {
            return self.tcp_write_raw(handle, data, timeout_ms).await;
        }
        match self.process_write_data(handle, data) {
            Ok(bytes) if bytes.is_empty() => (0, Ok(())),
            Ok(bytes) => self.tcp_write_raw(handle, &bytes, timeout_ms).await,
            Err(e) => (0, Err(e)),
        }
    }

    /// Write all of `data` to a TCP connection without HTTP parsing,
    /// returning how much of it the stream accepted, finished or not.
    async fn tcp_write_raw(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> (usize, Result<(), TcpError>) {
        let io_timeout = Self::effective_timeout(self.policy.config.io_timeout, timeout_ms);
        let Some(connection) = self.tcp_connections.get_mut(&handle) else {
            return (0, Err(TcpError::InvalidHandle));
        };
        let (accepted, written) = write_counted(connection.stream(), data, io_timeout, false).await;
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

    /// Close a TCP connection.
    pub fn tcp_close(&mut self, handle: u32) {
        if self.tcp_connections.remove(&handle).is_some() {
            tracing::debug!(handle, "TCP connection closed");
        }
        self.permits.remove(&handle);
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
        // A spent byte limit takes no handshakes either; the connection
        // stays the guest's.
        self.check_bytes_left().map_err(TlsError::Tcp)?;
        let connect_timeout =
            Self::effective_timeout(self.policy.config.connect_timeout, timeout_ms);

        // Remove the TCP connection (we're taking ownership). Its handle is
        // gone whether or not the handshake succeeds, so drop its state now.
        let tcp = self
            .tcp_connections
            .remove(&tcp_handle)
            .ok_or(TlsError::InvalidHandle)?;
        let host = self.connection_hosts.remove(&tcp_handle);
        let permit = self.permits.remove(&tcp_handle);
        self.http_states.remove(&tcp_handle);
        self.read_states.remove(&tcp_handle);

        let tls = self
            .policy
            .tls_connect(tcp, hostname, connect_timeout)
            .await?;

        let handle = self.alloc_handle();
        self.tls_connections.insert(handle, tls);
        if let Some(permit) = permit {
            self.permits.insert(handle, permit);
        }

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
        let io_timeout = Self::effective_timeout(self.policy.config.io_timeout, timeout_ms);
        let len = self.read_len(len).map_err(TlsError::Tcp)?;
        self.ensure_masker(handle).map_err(TlsError::Tcp)?;
        let stream = self
            .tls_connections
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;
        let masker = self
            .read_states
            .get_mut(&handle)
            .ok_or(TlsError::InvalidHandle)?;
        let data = masker
            .read(stream, len, io_timeout)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionReset => TlsError::Tcp(TcpError::ConnectionReset),
                std::io::ErrorKind::TimedOut => TlsError::Tcp(TcpError::TimedOut),
                _ => TlsError::Tcp(TcpError::IoError(e.to_string())),
            })?;
        Ok(self.charge_read(data))
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
        self.take_write(data.len()).map_err(TlsError::Tcp)?;
        let (accepted, written) = self.write_framed_tls(handle, data, timeout_ms).await;
        if written.is_err() {
            // A write that times out or fails may have sent some of it: only
            // what the stream never accepted is refunded.
            let unsent = data.len().saturating_sub(accepted);
            self.budget.bytes().refund_sent(unsent as u64);
        }
        written?;
        // Report what the guest wrote: substitution changes the length on the wire.
        Ok(data.len() as u32)
    }

    /// Write `data`, substituting secrets and checking HTTP rules if the
    /// connection frames requests, returning how many bytes the stream
    /// accepted, finished or not.
    async fn write_framed_tls(
        &mut self,
        handle: u32,
        data: &[u8],
        timeout_ms: u32,
    ) -> (usize, Result<(), TlsError>) {
        // Fast path: nothing to substitute or check = no parsing
        if !self.frames_requests(handle) {
            return self.tls_write_raw(handle, data, timeout_ms).await;
        }
        match self.process_write_data(handle, data) {
            Ok(bytes) if bytes.is_empty() => (0, Ok(())),
            Ok(bytes) => self.tls_write_raw(handle, &bytes, timeout_ms).await,
            Err(e) => (0, Err(e.into())),
        }
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
        let io_timeout = Self::effective_timeout(self.policy.config.io_timeout, timeout_ms);
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
        self.permits.remove(&handle);
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
    /// - `policy` (network policy, secrets and TLS roots)
    /// - `next_handle` (stays monotonic — stale guest handles remain invalid)
    pub fn close_all_connections(&mut self) {
        let tcp_count = self.tcp_connections.len();
        let tls_count = self.tls_connections.len();
        self.tcp_connections.clear();
        self.tls_connections.clear();
        self.permits.clear();
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
    if let Some(uri) = absolute_form(target) {
        // As it's written in `target` (see parse_target), borrowed from it.
        let start = uri.scheme_str()?.len() + "://".len();
        let authority = target.get(start..start + uri.authority()?.as_str().len())?;
        if authority.contains('@') {
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

/// Whether request target `target` is absolute-form (`scheme://host/...`).
fn is_absolute_form(target: &str) -> bool {
    absolute_form(target).is_some()
}

/// `target` parsed as a request target (RFC 9112's origin-, absolute- or
/// authority-form, or `*`), if the parse reads back as exactly `target`.
/// What HTTP rules check has to be what the server gets, so a target the
/// parser reads differently from its text is refused: one with a fragment
/// (dropped) or an upper-case scheme (lowered). The one difference allowed
/// is the `/` the parser gives an absolute URL with an empty path
/// (`http://host`, `http://host?q`), which is what an empty path means
/// (RFC 9112 section 3.2.2), so it's checked as `/`.
fn parse_target(target: &str) -> Option<http::Uri> {
    let uri: http::Uri = target.parse().ok()?;
    // Not `uri == target`, which ignores case in the scheme and authority.
    let read_back = uri.to_string();
    if read_back == target {
        return Some(uri);
    }
    let prefix = format!("{}://{}", uri.scheme_str()?, uri.authority()?);
    let rest = target.strip_prefix(&prefix)?;
    (rest.is_empty() || rest.starts_with('?'))
        .then(|| format!("{prefix}/{rest}"))
        .filter(|with_slash| *with_slash == read_back)
        .map(|_| uri)
}

/// `target` parsed, if it's absolute-form: a scheme at its very start (a
/// letter, then letters, digits, `+`, `-` or `.`; the parser takes any
/// scheme characters), `://` and an authority. A target with `://` anywhere
/// else (`../x?u=http://host/`, which a server may read as a relative path)
/// isn't.
fn absolute_form(target: &str) -> Option<http::Uri> {
    let uri = parse_target(target)?;
    let mut scheme = uri.scheme_str()?.chars();
    let scheme_ok = scheme.next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    (scheme_ok && uri.authority().is_some_and(plain_authority)).then_some(uri)
}

/// Whether `authority` is a plain `host[:port]`: a host name of letters,
/// digits, `-`, `.` and `_`, or a bracketed IPv6 address, and a port of
/// 0-65535. The parser takes more (`%` escapes, which a server may decode
/// into a path, like `..%2fsecret`; other sub-delims; zones).
fn plain_authority(authority: &http::uri::Authority) -> bool {
    let host = authority.host();
    let host_ok = match host.strip_circumfix('[', ']') {
        Some(v6) => v6.parse::<Ipv6Addr>().is_ok(),
        None => {
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        }
    };
    // `host()` leaves out any userinfo (`user@`), which has to be refused.
    let Some(rest) = authority.as_str().strip_prefix(host) else {
        return false;
    };
    let port_ok = match rest {
        "" => true,
        port => port.strip_prefix(':').is_some_and(|p| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()
        }),
    };
    host_ok && port_ok
}

/// The method and target of a request whose request line is exactly
/// `METHOD SP target SP HTTP/1.x`, and whose target contains no whitespace or
/// `\` and is in a request-target form ([`request_target_form`]).
fn request_line(headers: &str) -> Option<(&str, &str)> {
    let mut parts = headers.lines().next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || method.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || target.is_empty()
        || target.contains(|c: char| c.is_whitespace() || c == '\\')
        || !request_target_form(method, target)
    {
        return None;
    }
    Some((method, target))
}

/// Whether `target` is in one of RFC 9112's four request-target forms for
/// `method`: origin-form (`/path?query`), absolute-form (see
/// [`absolute_form`]), authority-form (`host:port`) for a CONNECT, or `*` for
/// an OPTIONS. Anything else is refused, since a server may resolve it in
/// ways the rules don't see: `../x?u=http://host/` looks absolute to a naive
/// check but is a relative path to a lenient server.
fn request_target_form(method: &str, target: &str) -> bool {
    let Some(uri) = parse_target(target) else {
        return false;
    };
    let path = uri.path_and_query().map(http::uri::PathAndQuery::as_str);
    match (uri.scheme(), uri.authority(), path) {
        (None, None, Some("*")) => method.eq_ignore_ascii_case("OPTIONS"),
        (None, None, Some(_)) => true,
        (None, Some(authority), None) => {
            method.eq_ignore_ascii_case("CONNECT") && plain_authority(authority)
        }
        (Some(_), Some(_), Some(_)) => absolute_form(target).is_some(),
        _ => false,
    }
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
/// than `chunked`, any `Transfer-Encoding` on an HTTP/1.0 request (RFC 9112
/// section 6.1), both that and `Content-Length`, or more than one length.
/// So is a `Host` that isn't a plain `host[:port]` ([`plain_authority`]),
/// since the host checks read it as a host and a port, and a server might
/// read anything else differently.
fn request_body(headers: &[u8]) -> Result<Body, TcpError> {
    let ambiguous = |what: &str| TcpError::NotPermitted(format!("ambiguous HTTP request: {what}"));
    let text = std::str::from_utf8(headers).map_err(|_| ambiguous("headers are not UTF-8"))?;
    let lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| ambiguous("unterminated headers"))?
        .split("\r\n");
    let (mut length, mut chunked, mut http_1_0) = (None, false, false);
    for (i, line) in lines.enumerate() {
        if line.contains(['\r', '\n']) {
            return Err(ambiguous("bare CR or LF"));
        }
        if i == 0 {
            // The request line, which callers have checked with
            // `request_line`, so it ends in the version.
            http_1_0 = line.ends_with(" HTTP/1.0");
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .filter(|(name, _)| !name.is_empty() && name.bytes().all(is_tchar))
            .ok_or_else(|| ambiguous("malformed header line"))?;
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("host") {
            let plain = value
                .parse::<http::uri::Authority>()
                .is_ok_and(|a| a.as_str() == value && plain_authority(&a));
            if !plain {
                return Err(ambiguous("Host is not a plain host[:port]"));
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if http_1_0 {
                return Err(ambiguous("Transfer-Encoding on an HTTP/1.0 request"));
            }
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
    use base64::Engine as _;
    use base64::prelude::BASE64_STANDARD;
    use tokio::io::AsyncReadExt as _;
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

    use super::policy::{HttpParsingState, ReadState, read_masked};
    use super::response::{CARRY_GRACE, RequestKind};
    use super::*;

    /// The request line [`NetPolicy::substitute_secrets_in_text`] is given
    /// where it doesn't matter: a secret without HTTP rules ignores it.
    const ANY_REQUEST: (&str, &str) = ("GET", "/");

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
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        // Default is allow_all_hosts=false with empty allowed_hosts, so nothing is permitted.
        assert!(manager.policy.check_host("localhost").is_err());
        assert!(manager.policy.check_host("127.0.0.1").is_err());
        assert!(manager.policy.check_host("192.168.1.1").is_err());
        assert!(manager.policy.check_host("10.0.0.1").is_err());
        assert!(manager.policy.check_host("google.com").is_err());
        assert!(manager.policy.check_host("api.example.com").is_err());
    }

    #[test]
    fn test_allowed_hosts_whitelist() {
        let config = NetConfig::default()
            .allow_host("*.example.com")
            .allow_host("api.github.com");
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        assert!(manager.policy.check_host("api.example.com").is_ok());
        assert!(manager.policy.check_host("api.github.com").is_ok());
        assert!(manager.policy.check_host("google.com").is_err());
    }

    #[test]
    fn test_allow_all_hosts_true() {
        let config = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        // allow_all_hosts=true permits all non-blocked hosts.
        assert!(manager.policy.check_host("google.com").is_ok());
        assert!(manager.policy.check_host("api.example.com").is_ok());
        // Blocked hosts are still blocked.
        assert!(manager.policy.check_host("localhost").is_err());
        assert!(manager.policy.check_host("127.0.0.1").is_err());
    }

    #[test]
    fn test_permissive_config() {
        let config = NetConfig::permissive();
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        assert!(manager.policy.check_host("localhost").is_ok());
        assert!(manager.policy.check_host("127.0.0.1").is_ok());
        assert!(manager.policy.check_host("google.com").is_ok());
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
        for host in [
            "example.com",
            "Example.COM.",
            "example.com:8080",
            "127.0.0.1:80",
            "[::1]",
            "[2001:db8::1]:443",
            "under_score.example",
        ] {
            let headers = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(matches!(body(&headers), Ok(Body::None)), "{host}");
        }
        for host in [
            "",
            "allowed.example:x@other.example",
            "user@example.com",
            "example.com/path",
            "example.com?q",
            "example.com:http",
            "example.com:99999",
            "example.com:",
            "::1",
            "[::1",
            "[not-v6]",
            "[fe80::1%25eth0]",
            "ex%61mple.com",
            "a b",
        ] {
            let headers = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(body(&headers).is_err(), "{host:?}");
        }
        // RFC 9112 section 6.1: Transfer-Encoding on HTTP/1.0 is faulty
        // framing, whether or not Content-Length is there too.
        assert!(matches!(
            body("POST / HTTP/1.0\r\nContent-Length: 5\r\n\r\n"),
            Ok(Body::Length(5))
        ));
        for headers in [
            "POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.0\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: identity\r\n\r\n",
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

    /// The guest's own connections (`eryx:net`) refuse the same heads as
    /// the child proxy: a `Host` that isn't a plain authority, and chunked
    /// on HTTP/1.0.
    #[test]
    fn guest_requests_need_plain_hosts_and_1_1_chunking() {
        let mut manager = ConnectionManager::new(NetConfig::permissive(), HashMap::new()).unwrap();
        let mut write = |request: &str| {
            let handle = manager.alloc_handle();
            manager
                .connection_hosts
                .insert(handle, "api.example.com".to_string());
            manager.process_write_data(handle, request.as_bytes())
        };
        let request = "GET / HTTP/1.1\r\nHost: api.example.com:8080\r\n\r\n";
        assert_eq!(write(request).unwrap(), request.as_bytes());
        for request in [
            "GET / HTTP/1.1\r\nHost: api.example.com:x@other.example\r\n\r\n",
            "POST / HTTP/1.0\r\nHost: api.example.com\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(
                matches!(write(request), Err(TcpError::NotPermitted(_))),
                "{request:?}"
            );
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // Test substitution for allowed host
        let result = manager.policy.substitute_secrets_in_text(
            "Bearer ERYX_SECRET_PLACEHOLDER_abc123",
            "Authorization",
            "api.example.com",
            ANY_REQUEST,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer real-secret-value");

        // Test rejection for disallowed host
        let result = manager.policy.substitute_secrets_in_text(
            "Bearer ERYX_SECRET_PLACEHOLDER_abc123",
            "Authorization",
            "evil.com",
            ANY_REQUEST,
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
        )
        .unwrap();

        assert_eq!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_HEADER",
                    "x-api-key",
                    "api.example.com",
                    ANY_REQUEST
                )
                .unwrap(),
            "real-secret-value"
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "Bearer PH_HEADER",
                    "Authorization",
                    "api.example.com",
                    ANY_REQUEST
                )
                .is_err()
        );
    }

    /// A secret with HTTP rules is only substituted in requests one of them
    /// matches, by host, method, path and query; a request with it anywhere
    /// else is refused. Git's smart HTTP fetch is the motivating case: the
    /// password reaches the two fetch endpoints, in `Authorization`, and
    /// nothing else on the host.
    #[test]
    fn secret_scoped_by_http_rules() {
        let git = |rules: Vec<HttpRule>| {
            crate::secrets::SecretConfig {
                placeholder: "PH".to_string(),
                ..crate::secrets::SecretConfig::new(
                    "real",
                    vec!["git.example.com".into(), "other.example.com".into()],
                )
            }
            .only_in_header("Authorization")
            .with_http_rules(rules)
        };
        let fetch_rules = vec![
            HttpRule::new("git.example.com")
                .with_methods(["GET"])
                .with_paths(["/org/repo.git/info/refs"])
                .with_queries(["service=git-upload-pack"]),
            HttpRule::new("git.example.com")
                .with_methods(["POST"])
                .with_paths(["/org/repo.git/git-upload-pack"]),
        ];
        // Each request on a new connection: a refused one ends its own.
        let send = |secret: &crate::secrets::SecretConfig, host: &str, request: &str| {
            let mut m = ConnectionManager::new(
                NetConfig::permissive().allow_plaintext_secrets(true),
                HashMap::from([("TOKEN".into(), secret.clone())]),
            )
            .unwrap();
            m.connection_hosts.insert(1, host.into());
            m.process_write_data(1, request.as_bytes())
                .map(|out| String::from_utf8(out).unwrap())
        };
        let request = |line: &str, header: &str| {
            format!("{line} HTTP/1.1\r\nHost: git.example.com\r\n{header}\r\n\r\n")
        };
        let basic = |creds: &str| format!("Authorization: Basic {}", BASE64_STANDARD.encode(creds));

        let secret = git(fetch_rules.clone());
        for line in [
            "GET /org/repo.git/info/refs?service=git-upload-pack",
            "POST /org/repo.git/git-upload-pack",
            "GET https://git.example.com/org/repo.git/info/refs?service=git-upload-pack",
        ] {
            let out = send(&secret, "git.example.com", &request(line, &basic("git:PH"))).unwrap();
            assert!(out.contains(&basic("git:real")), "{line}: {out}");
        }
        for line in [
            // A push.
            "GET /org/repo.git/info/refs?service=git-receive-pack",
            "POST /org/repo.git/git-receive-pack",
            // Another method, path or query on the same host.
            "POST /org/repo.git/info/refs?service=git-upload-pack",
            "GET /org/repo.git/info/refs",
            "GET /org/other.git/info/refs?service=git-upload-pack",
            "GET /org/repo.git/info/refs/../../other.git/info/refs?service=git-upload-pack",
            "GET /org/repo.git/info/refs?service=git-upload-pack&service=git-receive-pack",
        ] {
            let err = send(&secret, "git.example.com", &request(line, &basic("git:PH")));
            assert!(err.is_err(), "{line}: {err:?}");
        }
        // The header limit still applies to a request in scope.
        let line = "POST /org/repo.git/git-upload-pack";
        assert!(send(&secret, "git.example.com", &request(line, "X-Token: PH")).is_err());
        // The rules' host has to match too: the secret is allowed on
        // other.example.com, but no rule covers it.
        assert!(
            send(
                &secret,
                "other.example.com",
                &request(line, &basic("git:PH")).replace("git.example.com", "other.example.com")
            )
            .is_err()
        );
        // A request without the placeholder is untouched by the rules.
        let line = "GET /anything";
        assert!(send(&secret, "git.example.com", &request(line, "X-Other: 1")).is_ok());

        // No rules: any request to an allowed host, as before rules existed.
        let unscoped = git(vec![]);
        let line = "DELETE /org/repo.git/anything?x";
        let out = send(
            &unscoped,
            "git.example.com",
            &request(line, "Authorization: Bearer PH"),
        )
        .unwrap();
        assert!(out.contains("Authorization: Bearer real"), "{out}");
    }

    /// A secret's HTTP rules are validated with the hosts it can reach.
    #[test]
    fn secret_http_rules_are_validated() {
        let secrets = |rule: HttpRule| {
            HashMap::from([(
                "TOKEN".to_string(),
                crate::secrets::SecretConfig::new("real", vec!["h".into()]).with_http_rules([rule]),
            )])
        };
        let config = NetConfig::default();
        assert!(
            config
                .check_secret_hosts(&secrets(HttpRule::new("h").with_paths(["/a"])))
                .is_ok()
        );
        for rule in [
            HttpRule::new("h").with_paths(["/a/../b"]),
            HttpRule::new("h").with_queries(["("]),
        ] {
            let err = config.check_secret_hosts(&secrets(rule)).unwrap_err();
            assert!(err.contains("secret 'TOKEN'"), "{err}");
        }
    }

    /// Placeholders inside base64 `Basic` credentials are substituted and
    /// re-encoded, under the same host and header rules.
    #[test]
    fn secret_in_basic_credentials() {
        let basic = |s: &str| format!("Basic {}", BASE64_STANDARD.encode(s));
        let mut m = framing_manager();

        assert_eq!(
            m.policy
                .substitute_secrets_in_text(
                    &basic("git:PH"),
                    "authorization",
                    "api.example.com",
                    ANY_REQUEST
                )
                .unwrap(),
            basic("git:real")
        );
        assert!(
            m.policy
                .substitute_secrets_in_text(
                    &basic("git:PH"),
                    "Authorization",
                    "evil.com",
                    ANY_REQUEST
                )
                .is_err()
        );
        // Other headers and schemes aren't decoded.
        let encoded = BASE64_STANDARD.encode("git:PH");
        assert_eq!(
            m.policy
                .substitute_secrets_in_text(
                    &format!("Basic {encoded}"),
                    "X-Auth",
                    "api.example.com",
                    ANY_REQUEST
                )
                .unwrap(),
            format!("Basic {encoded}")
        );
        // Credentials without a placeholder are passed through as written.
        assert_eq!(
            m.policy
                .substitute_secrets_in_text(
                    "basic Z2l0OnB3",
                    "Authorization",
                    "api.example.com",
                    ANY_REQUEST
                )
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
        Arc::get_mut(&mut m.policy)
            .unwrap()
            .secrets
            .get_mut("KEY")
            .unwrap()
            .header = Some("X-Api-Key".into());
        assert!(
            m.policy
                .substitute_secrets_in_text(
                    &basic("git:PH"),
                    "Authorization",
                    "api.example.com",
                    ANY_REQUEST
                )
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
                    http_rules: Vec::new(),
                },
            )])
        };

        let permissive = NetConfig::permissive();
        assert!(permissive.check_secret_hosts(&secret(vec![])).is_err());
        let manager = ConnectionManager::new(permissive, secret(vec![])).unwrap();
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "any-host.com",
                    ANY_REQUEST
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
        let manager = ConnectionManager::new(allowlist, secret(vec![])).unwrap();
        assert_eq!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "api.example.com",
                    ANY_REQUEST
                )
                .unwrap(),
            "Bearer real-secret-value"
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "Bearer PLACEHOLDER_XYZ",
                    "Authorization",
                    "other.example.com",
                    ANY_REQUEST
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // All of these unauthorized hosts must be rejected
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "evil.com", ANY_REQUEST)
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "127.0.0.1", ANY_REQUEST)
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "localhost", ANY_REQUEST)
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_AUTH", "Authorization", "", ANY_REQUEST)
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
                http_rules: Vec::new(),
            },
        );
        secrets.insert(
            "GITHUB".to_string(),
            crate::secrets::SecretConfig {
                real_value: "github-secret".to_string(),
                placeholder: "PH_GITHUB".to_string(),
                allowed_hosts: vec!["api.github.com".to_string()],
                header: None,
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // OpenAI key for OpenAI host: OK
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_OPENAI",
                    "Authorization",
                    "api.openai.com",
                    ANY_REQUEST
                )
                .is_ok()
        );
        // OpenAI key for GitHub host: REJECTED
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_OPENAI",
                    "Authorization",
                    "api.github.com",
                    ANY_REQUEST
                )
                .is_err()
        );
        // GitHub key for GitHub host: OK
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_GITHUB",
                    "Authorization",
                    "api.github.com",
                    ANY_REQUEST
                )
                .is_ok()
        );
        // GitHub key for OpenAI host: REJECTED
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_GITHUB",
                    "Authorization",
                    "api.openai.com",
                    ANY_REQUEST
                )
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_GLOB",
                    "Authorization",
                    "api.openai.com",
                    ANY_REQUEST
                )
                .is_ok()
        );
        // Bare domain doesn't match *.openai.com
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_GLOB", "Authorization", "openai.com", ANY_REQUEST)
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_GLOB", "Authorization", "evil.com", ANY_REQUEST)
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // Deceptive domains that look similar but shouldn't match
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_DECEPTIVE",
                    "Authorization",
                    "api.openai.com.evil.com",
                    ANY_REQUEST
                )
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_DECEPTIVE",
                    "Authorization",
                    "api-openai.com",
                    ANY_REQUEST
                )
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_DECEPTIVE",
                    "Authorization",
                    "evil-api.openai.com",
                    ANY_REQUEST
                )
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_DECEPTIVE",
                    "Authorization",
                    "xapi.openai.com",
                    ANY_REQUEST
                )
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // IP address should NOT match hostname restriction
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_IP", "Authorization", "104.18.7.192", ANY_REQUEST)
                .is_err()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text("PH_IP", "Authorization", "1.2.3.4", ANY_REQUEST)
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // DNS is case-insensitive, so these should all work
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_CASE",
                    "Authorization",
                    "API.OPENAI.COM",
                    ANY_REQUEST
                )
                .is_ok()
        );
        assert!(
            manager
                .policy
                .substitute_secrets_in_text(
                    "PH_CASE",
                    "Authorization",
                    "Api.OpenAI.Com",
                    ANY_REQUEST
                )
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
                http_rules: Vec::new(),
            },
        );

        let manager = ConnectionManager::new(config, secrets).unwrap();

        // Text without any placeholder should pass through unchanged, for any host
        let result = manager.policy.substitute_secrets_in_text(
            "Bearer some-other-token",
            "Authorization",
            "evil.com",
            ANY_REQUEST,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Bearer some-other-token");
    }

    /// Test that TLS upgrade preserves connection_hosts mapping.
    #[test]
    fn test_tls_upgrade_preserves_host_mapping() {
        let config = NetConfig::permissive();
        let secrets = HashMap::new();
        let mut manager = ConnectionManager::new(config, secrets).unwrap();

        // Simulate a TCP connection with host mapping and HTTP state
        let tcp_handle = 1_u32;
        let tls_handle = 2_u32;
        manager
            .connection_hosts
            .insert(tcp_handle, "example.com".to_string());
        manager.http_states.insert(
            tcp_handle,
            manager.policy.request_framer("example.com", false),
        );

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
                http_rules: Vec::new(),
            },
        );

        let mut manager = ConnectionManager::new(config, secrets).unwrap();

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
        let result = manager
            .policy
            .substitute_http_headers(large_headers.as_bytes(), &manager.connection_hosts[&handle]);
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
                http_rules: Vec::new(),
            },
        );

        let mut manager = ConnectionManager::new(config, secrets).unwrap();

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
        let result = manager
            .policy
            .substitute_http_headers(headers, &manager.connection_hosts[&tls_handle]);

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
                http_rules: Vec::new(),
            },
        );

        let mut manager = ConnectionManager::new(config, secrets).unwrap();

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
        let result = manager
            .policy
            .substitute_http_headers(large_headers.as_bytes(), &manager.connection_hosts[&handle]);
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
                http_rules: Vec::new(),
            },
        );

        let mut manager = ConnectionManager::new(config, secrets).unwrap();

        // Allocate handles and insert fake per-connection state.
        let h1 = manager.alloc_handle();
        let h2 = manager.alloc_handle();
        manager
            .connection_hosts
            .insert(h1, "a.example.com".to_string());
        manager
            .connection_hosts
            .insert(h2, "b.example.com".to_string());
        let framer = |host| manager.policy.request_framer(host, false);
        let (f1, f2) = (framer("a.example.com"), framer("b.example.com"));
        manager.http_states.insert(h1, f1);
        manager.http_states.insert(h2, f2);

        // Create a real TCP pair so tcp_connections has entries.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, _server) = tokio::join!(tokio::net::TcpStream::connect(addr), async {
            listener.accept().await.unwrap().0
        },);
        manager
            .tcp_connections
            .insert(h1, CheckedTcp::unchecked(client.unwrap(), "a.example.com"));

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
        assert!(manager.policy.secrets.contains_key("KEY"));
        assert!(manager.policy.config.allow_all_hosts);
    }

    #[tokio::test]
    async fn stale_handles_are_invalid_after_close_all() {
        let config = NetConfig::permissive();
        let mut manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        // Create a real connection and store it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, _server) = tokio::join!(tokio::net::TcpStream::connect(addr), async {
            listener.accept().await.unwrap().0
        },);
        let handle = manager.alloc_handle();
        manager.tcp_connections.insert(
            handle,
            CheckedTcp::unchecked(client.unwrap(), "example.com"),
        );
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
        let mut manager = ConnectionManager::new(NetConfig::permissive(), HashMap::new()).unwrap();
        let next_before = manager.next_handle;

        manager.close_all_connections();

        assert_eq!(manager.connection_count(), 0);
        assert_eq!(manager.next_handle, next_before);
    }

    #[test]
    fn handle_counter_advances_after_close_all() {
        let mut manager = ConnectionManager::new(NetConfig::permissive(), HashMap::new()).unwrap();
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
        let manager = ConnectionManager::new(NetConfig::default(), HashMap::new()).unwrap();
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
            assert!(
                !manager.policy.is_ip_allowed(ip(blocked)),
                "{blocked} allowed"
            );
        }
        for public in [
            "8.8.8.8",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "64:ff9b::808:808",
        ] {
            assert!(manager.policy.is_ip_allowed(ip(public)), "{public} blocked");
        }
    }

    #[test]
    fn test_allowed_networks_override_blocked() {
        let config = NetConfig::default().allow_network("10.96.0.0/12".parse().unwrap());
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();
        assert!(manager.policy.is_ip_allowed(ip("10.96.12.34")));
        assert!(manager.policy.is_ip_allowed(ip("::ffff:10.96.12.34")));
        assert!(!manager.policy.is_ip_allowed(ip("10.0.0.1")));
    }

    /// A configured IPv4-mapped CIDR still matches, though addresses are also
    /// checked as IPv4.
    #[test]
    fn test_mapped_ipv6_networks_match() {
        let config = NetConfig {
            blocked_networks: vec!["::ffff:8.8.8.0/120".parse().unwrap()],
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();
        assert!(!manager.policy.is_ip_allowed(ip("::ffff:8.8.8.8")));
        assert!(manager.policy.is_ip_allowed(ip("8.8.4.4")));
    }

    /// IPv4-compatible and Teredo addresses are checked as the IPv4 address
    /// they embed, like NAT64.
    #[test]
    fn ipv4_compatible_and_teredo_match_embedded_ipv4() {
        let config = NetConfig {
            blocked_networks: vec!["10.0.0.0/8".parse().unwrap()],
            ..NetConfig::default()
        };
        let manager = ConnectionManager::new(config, HashMap::new()).unwrap();
        assert!(!manager.policy.is_ip_allowed(ip("::10.0.0.1")));
        // Teredo stores the client address inverted: !10.0.0.1 = f5ff:fffe.
        assert!(!manager.policy.is_ip_allowed(ip("2001:0:1:2:3:4:f5ff:fffe")));
        assert!(manager.policy.is_ip_allowed(ip("::8.8.8.8")));
        assert!(manager.policy.is_ip_allowed(ip("2001:0:1:2:3:4:f7f7:f7f7")));
    }

    #[test]
    fn test_allow_localhost_unblocks_loopback_networks_only() {
        let manager =
            ConnectionManager::new(NetConfig::default().allow_localhost(), HashMap::new()).unwrap();
        assert!(manager.policy.is_ip_allowed(ip("127.0.0.1")));
        assert!(manager.policy.is_ip_allowed(ip("::1")));
        assert!(!manager.policy.is_ip_allowed(ip("10.0.0.1")));
        assert!(!manager.policy.is_ip_allowed(ip("169.254.169.254")));
    }

    /// The hostname passes every pattern check, but its DNS answer is internal.
    #[tokio::test]
    async fn test_public_name_resolving_to_internal_ip_is_blocked() {
        let config = NetConfig {
            allow_all_hosts: true,
            ..NetConfig::default()
        }
        .with_resolver(FixedResolver(vec![ip("169.254.169.254")]));
        let mut manager = ConnectionManager::new(config, HashMap::new()).unwrap();

        let err = manager
            .tcp_connect("public.example", 80, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, TcpError::NotPermitted(m) if m.contains("resolves to a blocked network") && !m.contains("169.254")),
            "{err}"
        );
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
        let mut manager = ConnectionManager::new(config, HashMap::new()).unwrap();

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
                http_rules: Vec::new(),
            },
        );
        let mut manager = ConnectionManager::new(
            NetConfig::permissive().allow_plaintext_secrets(true),
            secrets,
        )
        .unwrap();
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

        let mut m = ConnectionManager::new(github_rules(), HashMap::new()).unwrap();
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

    /// A TCP write that times out keeps what the socket took charged to the
    /// byte limit, refunding only the rest.
    #[tokio::test]
    async fn timed_out_writes_refund_only_the_unsent_part() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = NetConfig::permissive().with_max_bytes(64 << 20);
        let mut m = ConnectionManager::new(config, HashMap::new()).unwrap();
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
        let used = (64 << 20) - m.budget.bytes().remaining().unwrap();
        assert!(used > 0 && used < 32 << 20, "{used}");
    }

    /// Once the byte limit is spent, a new connection and a TLS handshake are
    /// refused like reads and writes, and the refused upgrade leaves the
    /// connection with the guest.
    #[tokio::test]
    async fn a_spent_byte_limit_refuses_connects_and_handshakes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut m =
            ConnectionManager::new(NetConfig::permissive().with_max_bytes(4), HashMap::new())
                .unwrap();
        let (handle, _server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let handle = handle.unwrap();
        assert!(m.budget.bytes().take_sent(4));

        let limit = |e: &TcpError| matches!(e, TcpError::NotPermitted(m) if m.contains("network limit reached"));
        let err = m.tcp_connect("127.0.0.1", port, 0).await.unwrap_err();
        assert!(limit(&err), "{err}");
        let err = m.tls_upgrade(handle, "127.0.0.1", 0).await.unwrap_err();
        assert!(matches!(&err, TlsError::Tcp(e) if limit(e)), "{err}");
        assert!(m.tcp_connections.contains_key(&handle));
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

    /// A trailing dot names the same host (`pastebin.com.` is
    /// `pastebin.com`), so every host check treats the two alike: blocked
    /// hosts stay blocked, and allowed, internal and secret hosts still match.
    #[tokio::test]
    async fn trailing_dot_names_the_same_host_in_every_check() {
        let config = NetConfig::permissive()
            .block_host("pastebin.com")
            .block_host("*.internal.corp")
            .block_host("Evil.Example.");
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
        for host in [
            "pastebin.com",
            "pastebin.com.",
            "PASTEBIN.COM.",
            "db.internal.corp.",
            "evil.example",
            "evil.example.",
        ] {
            assert!(m.policy.check_host(host).is_err(), "{host} got through");
        }
        assert!(m.policy.check_host("example.com.").is_ok());

        let config = NetConfig::default()
            .allow_host("api.example.com")
            .allow_host("*.github.com");
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
        for host in ["api.example.com.", "API.github.com."] {
            assert!(m.policy.check_host(host).is_ok(), "{host} refused");
        }
        assert!(m.policy.check_host("example.com.").is_err());

        // Secrets go to their allowed hosts with or without the dot.
        let m = framing_manager();
        assert!(
            m.policy
                .substitute_secrets_in_text("PH", "authorization", "api.example.com.", ANY_REQUEST)
                .is_ok()
        );
        assert!(
            m.policy
                .substitute_secrets_in_text(
                    "PH",
                    "authorization",
                    "other.example.com.",
                    ANY_REQUEST
                )
                .is_err()
        );

        // An internal host skips the network checks with the dot too.
        let config = NetConfig::default()
            .allow_host("svc.internal")
            .with_internal_hosts(["svc.internal"])
            .with_resolver(Fixed);
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
        assert!(m.policy.resolve_allowed("svc.internal.", 443).await.is_ok());
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
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
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
            assert!(m.policy.check_host(host).is_err(), "{host} got through");
        }
        for host in [
            "127.0.0.2",
            "11.0.0.1",
            "::2",
            "example.com",
            "1.2.3.4.5",
            "08.1.2.3",
        ] {
            assert!(m.policy.check_host(host).is_ok(), "{host} refused");
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
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
        for host in [
            "db.internal.corp",
            "db.internal.corp.",
            "api.x.com",
            "api.x.com.",
        ] {
            assert!(m.policy.check_host(host).is_err(), "{host} got through");
        }
        assert!(m.policy.check_host("internal.corp").is_ok());

        assert!(same_host("::1", "[::1]"));
        assert!(host_matches_pattern("[::1]", "::1"));
        assert!(host_matches_pattern("::1", "[::1]"));

        let config = NetConfig::default()
            .allow_host("svc.internal")
            .allow_host("other.internal")
            .with_internal_hosts(["svc.internal"])
            .with_resolver(Fixed);
        let m = ConnectionManager::new(config, HashMap::new()).unwrap();
        for host in ["other.internal", "other.internal.", "svc.internal.evil"] {
            assert!(m.policy.resolve_allowed(host, 443).await.is_err(), "{host}");
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
    fn request_targets_must_be_in_a_request_target_form() {
        for (method, target) in [
            ("GET", "/x?y=http://z/"),
            ("GET", "http://host/x"),
            ("GET", "https://host/"),
            // An empty path is `/` (RFC 9112 section 3.2.2); proxy clients send it.
            ("GET", "http://example.com"),
            ("GET", "http://host?x"),
            ("GET", "http://[::1]:8080/x"),
            ("GET", "http://h_o-s.t:80/"),
            ("CONNECT", "host:443"),
            ("connect", "[::1]:443"),
            ("OPTIONS", "*"),
        ] {
            let line = format!("{method} {target} HTTP/1.1\r\n\r\n");
            assert_eq!(request_line(&line), Some((method, target)), "{line}");
        }
        for (method, target) in [
            ("GET", "../x?y=http://host/"),
            ("GET", "x"),
            ("GET", "*"),
            ("GET", "host:443"),
            ("GET", "http:///x"),
            ("GET", "HTTPS://host/"),
            ("GET", "HTTPS://host"),
            ("GET", "/x#y"),
            // Authorities must be a plain host[:port].
            ("GET", "http://..%2fsecret/x"),
            ("GET", "http://ho!st/"),
            ("GET", "http://host:99999/"),
            ("GET", "http://host:/x"),
            ("GET", "http://[fe80::1%25lo]/"),
            ("CONNECT", "..%2fx:443"),
            ("GET", "1http://host/"),
            ("CONNECT", "host:443/x"),
            ("OPTIONS", "*x"),
        ] {
            let line = format!("{method} {target} HTTP/1.1\r\n\r\n");
            assert_eq!(request_line(&line), None, "{line}");
        }
    }

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
            "http://api.github.com/user/keys?x=1",
            "svn+ssh://api.github.com/user/keys",
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
            // `://` that isn't right after a scheme at the start: a lenient
            // server can resolve these as relative paths (`/other`).
            "../other?x=https://api.github.com/repos/org/repo",
            "other?x=https://api.github.com/repos/org/repo",
            "1https://api.github.com/repos/org/repo",
            "-x://api.github.com/repos/org/repo",
            "https:///repos/org/repo",
            "://api.github.com/repos/org/repo",
            // Targets the parser reads differently from their text: what's
            // checked has to be what's sent.
            "https://api.github.com/repos/org/repo#x",
            "/repos/org/repo#x",
            "HTTPS://api.github.com/repos/org/repo",
            // An empty path is `/`, whatever the query says.
            "https://api.github.com?x=/repos/org/repo",
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
            let mut m = ConnectionManager::new(github_rules(), HashMap::new()).unwrap();
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
        let mut m = ConnectionManager::new(config, HashMap::new()).unwrap();
        m.connection_hosts.insert(1, "dav.example.com".into());
        let propfind = b"PROPFIND /files HTTP/1.1\r\nHost: dav.example.com\r\n\r\n";
        assert_eq!(m.process_write_data(1, propfind).unwrap(), propfind);
        assert!(
            m.process_write_data(1, b"MKCOL /files HTTP/1.1\r\nHost: dav.example.com\r\n\r\n")
                .is_err()
        );
    }

    /// Query patterns match the whole raw query: Git's smart HTTP fetch
    /// starts with the same request as a push but for its query.
    #[test]
    fn http_rules_match_queries() {
        let config = NetConfig::permissive().allow_http(
            HttpRule::new("git.example.com")
                .with_methods(["GET"])
                .with_paths(["/org/repo.git/info/refs"])
                .with_queries(["service=git-upload-pack"]),
        );
        let check = |target| {
            config
                .check_http_request("git.example.com", "GET", target)
                .is_ok()
        };
        assert!(check("/org/repo.git/info/refs?service=git-upload-pack"));
        assert!(check(
            "https://git.example.com/org/repo.git/info/refs?service=git-upload-pack"
        ));
        // Through a connection: a refused push never reaches the server.
        let mut m = ConnectionManager::new(config.clone(), HashMap::new()).unwrap();
        m.connection_hosts.insert(1, "git.example.com".into());
        let fetch = b"GET /org/repo.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: git.example.com\r\n\r\n";
        assert_eq!(frame(&mut m, &[fetch]), String::from_utf8_lossy(fetch));
        m.http_states.clear();
        let push = b"GET /org/repo.git/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: git.example.com\r\n\r\n";
        assert!(m.process_write_data(1, push).is_err());

        for target in [
            "/org/repo.git/info/refs?service=git-receive-pack",
            "/org/repo.git/info/refs",
            "/org/repo.git/info/refs?",
            // Anchored at both ends.
            "/org/repo.git/info/refs?service=git-upload-pack&x",
            "/org/repo.git/info/refs?x&service=git-upload-pack",
            // Repeated parameters and `;` separators, which servers read
            // differently, and escapes are just text that doesn't match.
            "/org/repo.git/info/refs?service=git-upload-pack&service=git-receive-pack",
            "/org/repo.git/info/refs?service=git-receive-pack;service=git-upload-pack",
            "/org/repo.git/info/refs?service=git%2Dupload-pack",
            "/org/repo.git/info/refs?service=git-upload-pack#x",
            // The path still has to match.
            "/org/repo.git/info/refs/../x?service=git-upload-pack",
        ] {
            assert!(!check(target), "{target}");
        }

        let check = |queries: &[&str], target| {
            NetConfig::permissive()
                .allow_http(HttpRule::new("h").with_queries(queries.iter().copied()))
                .check_http_request("h", "GET", target)
                .is_ok()
        };
        // No queries means any query, as before queries could be set.
        assert!(check(&[], "/x?anything=at&all"));
        assert!(check(&[], "/x"));
        // An optional group allows no query too; `?` alone is an empty query.
        let optional = ["(service=git-upload-pack)?"];
        assert!(check(&optional, "/x"));
        assert!(check(&optional, "/x?"));
        assert!(check(&optional, "/x?service=git-upload-pack"));
        assert!(!check(&optional, "/x?service=git-receive-pack"));
        // Any pattern may match; an alternation stays inside the anchors.
        assert!(check(&["a=1", "b=2"], "/x?b=2"));
        assert!(check(&["a=1|b=2"], "/x?a=1"));
        assert!(!check(&["a=1|b=2"], "/x?a=1&c"));
        assert!(!check(&["a=1|b=2"], "/x?c&b=2"));
    }

    /// A query pattern that doesn't compile is refused, including one that
    /// would escape its anchors once wrapped.
    #[test]
    fn http_rule_queries_are_validated() {
        let config = |q| NetConfig::default().allow_http(HttpRule::new("h").with_queries([q]));
        assert!(config("a=[0-9]+").validate().is_ok());
        for q in ["(", "a)|(b", "a)(?:", "\\"] {
            let err = config(q).validate().unwrap_err();
            assert!(err.contains("query pattern"), "{q}: {err}");
            // Nor does it match anything if checked anyway.
            assert!(
                config(q).check_http_request("h", "GET", "/?a").is_err(),
                "{q}"
            );
        }
        let rule = HttpRule::new("h").with_queries(["a"]);
        assert_eq!(rule.queries(), ["a"]);
        assert_eq!(rule, HttpRule::new("h").with_queries(["a"]));
        assert_ne!(rule, HttpRule::new("h"));
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
        let mut m = ConnectionManager::new(github_rules(), HashMap::new()).unwrap();
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
        // A target that only contains `://` isn't absolute-form, so its
        // "path" after it isn't what the rules check.
        let relative = b"GET ../other?x=http://api.github.com/repos/org/repo HTTP/1.1\r\nHost: api.github.com\r\n\r\n";
        assert!(m.process_write_data(1, relative).is_err());
        m.http_states.clear();
        // An authority a server may decode into a path, or any host other
        // than the connection's, isn't sent to a host with rules.
        for request in [
            &b"GET http://..%2fsecret/repos/org/repo HTTP/1.1\r\nHost: ..%2fsecret\r\n\r\n"[..],
            b"GET http://..%2fsecret/repos/org/repo HTTP/1.1\r\n\r\n",
            b"GET http://%2e%2e%2fsecret/repos/org/repo HTTP/1.1\r\n\r\n",
            b"GET /repos/org/repo HTTP/1.1\r\nHost: ..%2fsecret\r\n\r\n",
            b"GET /repos/org/repo HTTP/1.1\r\nHost: other.example\r\n\r\n",
        ] {
            assert!(m.process_write_data(1, request).is_err(), "{request:?}");
            m.http_states.clear();
        }
        // An absolute URL with an empty path, as proxy clients send it, goes
        // to a host without rules.
        for request in [
            &b"GET http://www.github.com HTTP/1.1\r\nHost: www.github.com\r\n\r\n"[..],
            b"GET http://www.github.com?x HTTP/1.1\r\nHost: www.github.com\r\n\r\n",
        ] {
            assert!(m.process_write_data(2, request).is_ok(), "{request:?}");
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

    /// A server that speaks first marks the connection raw (another
    /// protocol), but on a host with HTTP rules what the guest writes next
    /// is still refused unless it's a request the rules allow.
    #[tokio::test]
    async fn a_server_speaking_first_doesnt_skip_http_rules() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = framing_manager();
        let mut m = ConnectionManager::new(
            base.policy
                .config
                .clone()
                .allow_http(HttpRule::new("127.0.0.1").with_paths(["/public"])),
            base.policy.secrets.clone(),
        )
        .unwrap();
        let (handle, server) = tokio::join!(m.tcp_connect("127.0.0.1", port, 0), async {
            listener.accept().await.unwrap().0
        });
        let (handle, mut server) = (handle.unwrap(), server);
        server.write_all(b"hello\r\n").await.unwrap();
        // The read side has seen the server speak first.
        assert!(!m.tcp_read(handle, 64, 0).await.unwrap().is_empty());
        assert!(
            m.tcp_write(handle, b"GET /secret HTTP/1.1\r\nHost: other\r\n\r\n", 0)
                .await
                .is_err()
        );
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
                assert_eq!(
                    a,
                    b,
                    "{k} bytes of the {}-byte secret, trial {trial}",
                    secret.len()
                );
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
        let host = |h: &str| m.policy.read_masks(h);
        assert_eq!(host("api.example.com"), masks(&["real"]));
        assert!(host("echo.example.net").is_empty());
        // Nor on a connection whose requests don't get values substituted:
        // plain TCP without `allow_plaintext_secrets`.
        let plain =
            Arc::new(NetPolicy::new(NetConfig::permissive(), m.policy.secrets.clone()).unwrap());
        assert!(
            plain
                .request_framer("api.example.com", false)
                .response_masker()
                .masker
                .is_none()
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
