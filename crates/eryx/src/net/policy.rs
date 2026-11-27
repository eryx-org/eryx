//! The network policy, shared by every path sandboxed code reaches the
//! network through.
//!
//! Guests reach the network through `eryx:net` ([`ConnectionManager`]); the
//! child-process proxy (#812) will be a second caller. Both go through
//! [`NetPolicy`], so they can't drift apart:
//!
//! - [`NetPolicy::connect`] checks the host, resolves it once, checks every
//!   address and dials only the ones that passed. There's no separate
//!   resolve step to misuse, and [`NetPolicy::check_host`] on its own never
//!   reaches the network.
//! - [`NetPolicy::tls_connect`] binds the TLS server name to the connected
//!   host.
//! - A [`RequestFramer`] per connection frames the HTTP requests written to
//!   it, checks them against the HTTP rules and substitutes secrets.
//! - A [`ResponseMasker`] per connection, paired with its framer, frames the
//!   responses to those requests and replaces real secret values read back
//!   with their placeholders (see [`super::response`]).
//!
//! [`ConnectionManager`]: super::ConnectionManager

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::credential::Credential;
use super::response::{self, CARRY_GRACE, RequestKind, UNSOLICITED};
use super::{
    Body, IpNet, MAX_HEADER_LEN, NetConfig, TcpError, TlsError, embedded_ipv4, find_header_end,
    host_matches_pattern, is_absolute_form, is_http2, read_buf_len, request_authorities,
    request_body, request_line, request_matches_rules, request_start, same_host, strip_port,
    switches_protocols,
};
use crate::secrets::SecretConfig;

/// A sandbox's network policy: its [`NetConfig`], its secrets, and the TLS
/// roots it trusts.
#[derive(Debug)]
pub(crate) struct NetPolicy {
    pub(super) config: NetConfig,
    pub(super) secrets: HashMap<String, SecretConfig>,
    tls_config: Arc<ClientConfig>,
    /// The policy this one narrows (see [`NetPolicy::narrowed`]): every check
    /// must pass it too.
    parent: Option<Arc<NetPolicy>>,
}

impl NetPolicy {
    /// The policy for `config` and `secrets`.
    ///
    /// # Errors
    ///
    /// Fails, as [`NetConfig::validate`] does, if `config` is invalid: every
    /// policy is built from a valid config.
    pub(crate) fn new(
        config: NetConfig,
        secrets: HashMap<String, SecretConfig>,
    ) -> Result<Self, String> {
        config.validate()?;
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

        Ok(Self {
            config,
            secrets,
            tls_config: Arc::new(tls_config),
            parent: None,
        })
    }

    /// A policy that allows only what both `self` and `config` allow, e.g. a
    /// child process's narrower network inside its sandbox's.
    ///
    /// Hosts, resolved addresses and HTTP rules are checked against both,
    /// and the connection limit and timeouts are the smaller of the two.
    /// Secrets, their hosts, plaintext substitution, the resolver and the
    /// TLS roots come from `self`, so a narrowed policy can never reach or
    /// reveal more than `self`.
    ///
    /// # Errors
    ///
    /// Fails, as [`NetConfig::validate`] does, if `config` is invalid.
    pub(crate) fn narrowed(self: &Arc<Self>, config: NetConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            secrets: self.secrets.clone(),
            tls_config: Arc::clone(&self.tls_config),
            parent: Some(Arc::clone(self)),
        })
    }

    /// The outermost policy, which owns the secrets' settings.
    fn root(&self) -> &NetPolicy {
        self.parent.as_deref().map_or(self, NetPolicy::root)
    }

    /// This policy and the ones it narrows, innermost first.
    fn chain(&self) -> impl Iterator<Item = &NetPolicy> {
        std::iter::successors(Some(self), |p| p.parent.as_deref())
    }

    /// The most concurrent connections allowed (`0` for no limit): the
    /// smallest limit in the chain.
    pub(crate) fn max_connections(&self) -> u32 {
        self.chain()
            .map(|p| p.config.max_connections())
            .filter(|&max| max > 0)
            .min()
            .unwrap_or(0)
    }

    /// The most bytes an execution may move, if limited: the smallest limit
    /// in the chain.
    pub(crate) fn max_bytes(&self) -> Option<u64> {
        self.chain().filter_map(|p| p.config.max_bytes()).min()
    }

    /// The connect timeout: the shortest in the chain.
    pub(crate) fn connect_timeout(&self) -> Duration {
        self.chain()
            .map(|p| p.config.connect_timeout())
            .min()
            .unwrap_or_default()
    }

    /// The I/O timeout: the shortest in the chain.
    pub(crate) fn io_timeout(&self) -> Duration {
        self.chain()
            .map(|p| p.config.io_timeout())
            .min()
            .unwrap_or_default()
    }

    /// Whether any HTTP rule in the chain applies to `host` on `port`.
    pub(crate) fn has_http_rules(&self, host: &str, port: u16) -> bool {
        self.chain().any(|p| p.config.has_http_rules(host, port))
    }

    /// Whether the chain has any HTTP rules at all.
    fn any_http_rules(&self) -> bool {
        self.chain().any(|p| !p.config.http_rules.is_empty())
    }

    /// Check `host` on `port` against `blocked_hosts`/`allowed_hosts`.
    ///
    /// Passing doesn't make a host reachable: [`NetPolicy::connect`] checks
    /// it again, along with the addresses it resolves to.
    pub(crate) fn check_host(&self, host: &str, port: u16) -> Result<(), TcpError> {
        // Check blocked first (always enforced).
        for pattern in &self.config.blocked_hosts {
            if host_matches_pattern(host, port, pattern) {
                return Err(TcpError::NotPermitted(format!("host '{host}' is blocked")));
            }
        }

        // Unless allow_all_hosts is set, the host must match the allowlist.
        // Either way the policy this one narrows checks it too.
        if !self.config.allow_all_hosts
            && !self
                .config
                .allowed_hosts
                .iter()
                .any(|p| host_matches_pattern(host, port, p))
        {
            return Err(TcpError::NotPermitted(format!(
                "host '{host}' port {port} not in allowed list"
            )));
        }

        match &self.parent {
            Some(parent) => parent.check_host(host, port),
            None => Ok(()),
        }
    }

    /// Check a resolved address against `blocked_networks`/`allowed_networks`.
    ///
    /// An address matches a network in either its own form or the IPv4
    /// address it stands for, so `::ffff:10.0.0.0/104` and `10.0.0.0/8` both
    /// match `::ffff:10.0.0.1`.
    pub(super) fn is_ip_allowed(&self, ip: IpAddr) -> bool {
        let embedded = embedded_ipv4(ip);
        let matches = |nets: &[IpNet]| {
            nets.iter()
                .any(|n| n.contains(&ip) || embedded.is_some_and(|v4| n.contains(&v4)))
        };
        !matches(&self.config.blocked_networks) || matches(&self.config.allowed_networks)
    }

    /// Whether `host` may connect to `ip` on `port`: an internal host may
    /// reach any address, others only allowed ones, in every policy of the
    /// chain.
    fn address_allowed(&self, host: &str, port: u16, ip: IpAddr) -> bool {
        self.chain().all(|p| {
            p.config
                .internal_hosts
                .iter()
                .any(|pattern| host_matches_pattern(host, port, pattern))
                || p.is_ip_allowed(ip)
        })
    }

    /// Resolve `host` and keep the addresses the policy allows.
    ///
    /// Only [`NetPolicy::connect`] calls this, and it dials only the returned
    /// addresses, never re-resolving: the check is only as good as the
    /// address actually used.
    pub(super) async fn resolve_allowed(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, TcpError> {
        // The sandbox's resolver: a narrowed policy can't re-point a host.
        let addrs: Vec<SocketAddr> = match &self.root().config.resolver {
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

        // A child proxy is never a destination, whatever the policy allows
        // (see `proxy::is_child_proxy`).
        let (allowed, blocked): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| {
            self.address_allowed(host, port, a.ip()) && !super::proxy::is_child_proxy(*a)
        });
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

    /// Connect to `host:port` if the policy allows it.
    ///
    /// Checks the host, resolves it once and dials only the addresses that
    /// pass the network checks, so a DNS answer can't change between the
    /// check and the dial. Resolving and connecting each get `timeout`.
    pub(crate) async fn connect(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<CheckedTcp, TcpError> {
        self.check_host(host, port)?;
        let addrs = tokio::time::timeout(timeout, self.resolve_allowed(host, port))
            .await
            .map_err(|_| TcpError::TimedOut)??;

        // Tries each checked address in turn, never re-resolving.
        let stream = tokio::time::timeout(timeout, TcpStream::connect(&addrs[..]))
            .await
            .map_err(|_| TcpError::TimedOut)?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused => TcpError::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset => TcpError::ConnectionReset,
                std::io::ErrorKind::TimedOut => TcpError::TimedOut,
                _ => TcpError::IoError(e.to_string()),
            })?;
        Ok(CheckedTcp {
            stream,
            host: host.to_string(),
        })
    }

    /// Start TLS on `tcp`, verifying the server as `server_name`.
    ///
    /// The certificate is checked against `server_name`, and secrets against
    /// the host `tcp` was connected to, so the two must be the same host.
    pub(crate) async fn tls_connect(
        &self,
        tcp: CheckedTcp,
        server_name: &str,
        timeout: Duration,
    ) -> Result<TlsStream<TcpStream>, TlsError> {
        let CheckedTcp { stream: tcp, host } = tcp;
        if !same_host(&host, server_name) {
            return Err(TlsError::Tcp(TcpError::NotPermitted(format!(
                "TLS hostname '{server_name}' does not match the connected host"
            ))));
        }

        let connector = TlsConnector::from(self.tls_config.clone());
        let name: ServerName<'static> = server_name
            .to_string()
            .try_into()
            .map_err(|_| TlsError::HandshakeFailed("invalid hostname".into()))?;

        tokio::time::timeout(timeout, connector.connect(name, tcp))
            .await
            .map_err(|_| TlsError::Tcp(TcpError::TimedOut))?
            .map_err(|e| TlsError::HandshakeFailed(e.to_string()))
    }

    /// Whether secret placeholders are substituted on a connection: only
    /// `over_tls`, unless [`NetConfig::allow_plaintext_secrets`] is set, and
    /// only if there are secrets.
    fn substitutes_secrets(&self, over_tls: bool) -> bool {
        !self.secrets.is_empty() && (over_tls || self.root().config.allow_plaintext_secrets)
    }

    /// Whether a connection (`over_tls` or not) needs a [`RequestFramer`] at
    /// all: with no secrets to substitute and no HTTP rules, its bytes can
    /// be sent as written.
    pub(crate) fn frames_requests(&self, over_tls: bool) -> bool {
        self.any_http_rules() || self.substitutes_secrets(over_tls)
    }

    /// A framer for the requests written to a connection to `host` on
    /// `port`, sent `over_tls` or in plaintext.
    pub(crate) fn request_framer(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        over_tls: bool,
    ) -> RequestFramer {
        RequestFramer {
            policy: Arc::clone(self),
            host: host.to_string(),
            port,
            over_tls,
            credentials: Vec::new(),
            substitute: self.substitutes_secrets(over_tls),
            http_only: false,
            authority: String::new(),
            state: Arc::default(),
        }
    }

    /// `(real value, placeholder)` of each secret that may be sent to `host`
    /// on `port` (the sandbox's, the root policy's, secret hosts), the only
    /// secrets its responses can legitimately contain.
    pub(super) fn read_masks(&self, host: &str, port: u16) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.secrets
            .values()
            .filter(|s| !s.real_value.is_empty())
            .filter(|s| {
                self.root()
                    .config
                    .secret_hosts(s)
                    .iter()
                    .any(|p| host_matches_pattern(host, port, p))
            })
            .map(|s| {
                (
                    s.real_value.clone().into_bytes(),
                    s.placeholder.clone().into_bytes(),
                )
            })
            .collect()
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
    /// they're refused); with `http_only`, never. A leading empty line,
    /// which servers skip before a request, and a malformed request line are
    /// always refused.
    fn classify(
        &self,
        host: &str,
        port: u16,
        state: &mut HttpParsingState,
        http_only: bool,
    ) -> Result<Option<bool>, TcpError> {
        if state.buffer.starts_with(b"\r") || state.buffer.starts_with(b"\n") {
            return Err(TcpError::NotPermitted(
                "HTTP request starts with an empty line".into(),
            ));
        }
        match request_start(&state.buffer) {
            Some(false) if http_only => Err(TcpError::NotPermitted(format!(
                "only HTTP/1.1 requests may be sent to {host}"
            ))),
            Some(false)
                if self.any_http_rules() && (state.started || self.has_http_rules(host, port)) =>
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
    /// for the connected `host` and every host the request names, all on
    /// the connected `port`. With `http_only`, the request may only name the
    /// connected host.
    fn check_http_rules(
        &self,
        headers: &[u8],
        host: &str,
        port: u16,
        http_only: bool,
    ) -> Result<(), TcpError> {
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
        if http_only && !named.iter().all(|a| same_host(strip_port(a), host)) {
            return Err(TcpError::NotPermitted(format!(
                "HTTP request names a host other than {host}"
            )));
        }
        // A host with rules only gets requests naming itself: a server may
        // route by, or decode into the path, an authority the rules weren't
        // checked against (`http://..%2fsecret/public`, `Host: other`).
        if self.has_http_rules(host, port) && !named.iter().all(|a| same_host(strip_port(a), host))
        {
            return Err(TcpError::NotPermitted(
                "HTTP request names a host other than the connection's, which has HTTP rules"
                    .into(),
            ));
        }
        for host in std::iter::once(host).chain(named.into_iter().map(strip_port)) {
            for policy in self.chain() {
                policy
                    .config
                    .check_http_request(host, port, method, target)
                    .map_err(TcpError::NotPermitted)?;
            }
        }
        Ok(())
    }

    /// Substitute secret placeholders in HTTP headers sent to `target_host`
    /// on `port`.
    pub(super) fn substitute_http_headers(
        &self,
        headers: &[u8],
        target_host: &str,
        port: u16,
    ) -> Result<Vec<u8>, TcpError> {
        let text = String::from_utf8_lossy(headers);

        // The server routes by the Host header (or an absolute-form target),
        // so a request naming another host must not carry secrets.
        let names_target = request_authorities(&text)
            .is_some_and(|hosts| hosts.iter().all(|a| same_host(strip_port(a), target_host)));
        if !names_target && self.secrets.values().any(|s| text.contains(&s.placeholder)) {
            return Err(TcpError::NotPermitted(
                "Secret not allowed: request names a different host than the connection".into(),
            ));
        }

        // What a secret's HTTP rules match. A malformed request line was
        // refused before this; if one got here, no rule would match it.
        let request = request_line(&text).unwrap_or_default();

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
                    self.substitute_secrets_in_text(value, name, target_host, port, request)?;
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
    /// substituted. A secret limited to one header is only substituted there,
    /// and one with HTTP rules only in a `request` (method and target) one
    /// of them matches.
    ///
    /// Placeholders in `Authorization: Basic` credentials, which clients
    /// base64-encode, are substituted in the decoded `user:password` and the
    /// result re-encoded.
    pub(super) fn substitute_secrets_in_text(
        &self,
        text: &str,
        header: &str,
        target_host: &str,
        port: u16,
        request: (&str, &str),
    ) -> Result<String, TcpError> {
        if header.eq_ignore_ascii_case("authorization")
            && let Some((scheme, token)) = text.split_once(' ')
            && scheme.eq_ignore_ascii_case("basic")
            && let Ok(decoded) = BASE64_STANDARD.decode(token.trim())
            && let Ok(credentials) = String::from_utf8(decoded)
        {
            let substituted =
                self.substitute_placeholders(&credentials, header, target_host, port, request)?;
            if substituted != credentials {
                return Ok(format!("Basic {}", BASE64_STANDARD.encode(substituted)));
            }
        }
        self.substitute_placeholders(text, header, target_host, port, request)
    }

    /// Substitute secret placeholders appearing verbatim in `text`, the value
    /// of the header `header`, as described in
    /// [`substitute_secrets_in_text`](Self::substitute_secrets_in_text).
    fn substitute_placeholders(
        &self,
        text: &str,
        header: &str,
        target_host: &str,
        port: u16,
        (method, target): (&str, &str),
    ) -> Result<String, TcpError> {
        let mut result = text.to_string();

        for secret_config in self.secrets.values() {
            if !text.contains(&secret_config.placeholder) {
                continue;
            }

            // The secret's hosts are the sandbox's (the root policy's).
            let host_allowed = self
                .root()
                .config
                .secret_hosts(secret_config)
                .iter()
                .any(|p| host_matches_pattern(target_host, port, p));

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
            if !request_matches_rules(&secret_config.http_rules, target_host, port, method, target)
            {
                return Err(TcpError::NotPermitted(format!(
                    "Secret not allowed for this request: {method} {target}"
                )));
            }

            // Substitute
            result = result.replace(&secret_config.placeholder, &secret_config.real_value);
        }

        Ok(result)
    }
}

/// A TCP connection [`NetPolicy::connect`] made, with the host it checked,
/// which [`NetPolicy::tls_connect`] binds the TLS server name to.
#[derive(Debug)]
pub(crate) struct CheckedTcp {
    stream: TcpStream,
    host: String,
}

impl CheckedTcp {
    pub(crate) fn stream(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// The stream, for a caller done with the checked host.
    pub(crate) fn into_stream(self) -> TcpStream {
        self.stream
    }

    /// Wrap a stream the policy didn't check, for tests of what holds one.
    #[cfg(test)]
    pub(super) fn unchecked(stream: TcpStream, host: &str) -> Self {
        Self {
            stream,
            host: host.to_string(),
        }
    }
}

/// HTTP request framing state for a single connection.
#[derive(Debug, Default)]
pub(super) struct HttpParsingState {
    /// Request bytes buffered until the headers are complete.
    pub(super) buffer: Vec<u8>,
    /// How much of the current request's body is still to come.
    pub(super) body: Body,
    /// Whether a request has been framed on this connection yet.
    pub(super) started: bool,
    /// Whether everything on this connection is passed through unparsed
    /// from now on: its first bytes weren't a request, or a `CONNECT` or
    /// `Upgrade` without HTTP rules ended (see [`NetConfig::allow_http`]).
    pub(super) raw: bool,
    /// Whether the current request is a `CONNECT` or asks for an `Upgrade`:
    /// once it ends, what follows may be another protocol, or go to another
    /// host, so it's never parsed (or substituted) as requests.
    pub(super) switches: bool,
    /// Whether the one empty line allowed between requests was skipped.
    pub(super) skipped_crlf: bool,
    /// Whether a write was refused, after which every write is.
    pub(super) failed: bool,
    /// The kinds of the requests sent and still awaiting a response, which
    /// the connection's [`ResponseMasker`] frames those responses by.
    pub(super) requests: VecDeque<RequestKind>,
}

/// Lock a connection's framing state, which its framer (writes) and masker
/// (reads) share. Neither holds it across an `await`, and a panic while
/// holding it can't leave it worse than the bytes it describes, so a
/// poisoned lock is used as is.
fn lock(state: &Mutex<HttpParsingState>) -> MutexGuard<'_, HttpParsingState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Frames the HTTP requests written to one connection, checking each against
/// the HTTP rules and substituting secret placeholders in its headers.
///
/// Each connection gets its own, from [`NetPolicy::request_framer`].
#[derive(Debug)]
pub(crate) struct RequestFramer {
    policy: Arc<NetPolicy>,
    /// The host the connection was made to.
    host: String,
    /// The port the connection was made to.
    port: u16,
    /// Whether the connection is TLS (the proxy's terminated TLS).
    over_tls: bool,
    /// The credentials for this connection's host and port (see
    /// [`RequestFramer::with_credentials`]).
    credentials: Vec<Arc<Credential>>,
    /// Whether secret placeholders are substituted.
    substitute: bool,
    /// Whether anything but HTTP/1.1 requests is refused (see
    /// [`RequestFramer::http_only`]).
    http_only: bool,
    /// For the child proxy, the `host[:port]` the connection was made to,
    /// for requests forwarded with no `Host` header.
    authority: String,
    /// Shared with the connection's [`ResponseMasker`], which frames the
    /// responses by the requests framed here.
    state: Arc<Mutex<HttpParsingState>>,
}

impl RequestFramer {
    /// Refuse anything but HTTP/1.1 requests, from the first byte, as if
    /// every host had HTTP rules, and forward each in origin-form (see
    /// [`to_origin_form`]). For the child proxy, which forwards nothing it
    /// can't check, to `authority` (`host[:port]`).
    pub(crate) fn http_only(mut self, authority: impl Into<String>) -> Self {
        self.http_only = true;
        self.authority = authority.into();
        self
    }

    /// Add those of `credentials` that are for this connection's host and
    /// port to its requests (see [`inject_credentials`]), and mask them out
    /// of its responses. The connection then takes nothing but HTTP/1.1
    /// requests naming its host, as with [`http_only`](Self::http_only), and
    /// a request one applies to that it can't be sent over (plain HTTP,
    /// without [`Credential::allow_plaintext`]) is refused.
    pub(crate) fn with_credentials(mut self, credentials: &[Arc<Credential>]) -> Self {
        self.credentials = credentials
            .iter()
            .filter(|c| c.for_host(&self.host, self.port))
            .cloned()
            .collect();
        self
    }

    /// Run bytes written to the connection through request framing, and
    /// return the bytes to send now: empty while a request's headers are
    /// still incomplete.
    ///
    /// # Errors
    ///
    /// Refuses a request the policy doesn't allow; nothing from that call is
    /// to be sent, and every later call fails too.
    pub(crate) fn frame(&mut self, data: &[u8]) -> Result<Vec<u8>, TcpError> {
        let shared = Arc::clone(&self.state);
        let mut state = lock(&shared);
        if state.failed {
            return Err(TcpError::NotPermitted(
                "an earlier write on this connection was refused".into(),
            ));
        }
        let result = self.frame_requests(&mut state, data);
        // After an error the buffered bytes are dropped, and the connection
        // refuses further writes: its framing can't be trusted any more.
        // The requests already sent still get their responses framed.
        if result.is_err() {
            *state = HttpParsingState {
                failed: true,
                started: state.started,
                requests: std::mem::take(&mut state.requests),
                ..HttpParsingState::default()
            };
        }
        result
    }

    /// The masker for the responses read from this framer's connection.
    /// Only a connection whose requests get real values substituted has its
    /// responses framed and masked, with the secrets that may be sent to its
    /// host: anywhere else none was ever sent.
    pub(crate) fn response_masker(&self) -> ResponseMasker {
        let mut masks = if self.substitute {
            self.policy.read_masks(&self.host, self.port)
        } else {
            Vec::new()
        };
        masks.extend(self.credentials.iter().flat_map(|c| c.masks()));
        let masker = response::ResponseMasker::new(masks)
            .map(|m| m.carrying_credential(!self.credentials.is_empty()));
        ResponseMasker {
            masker,
            error: None,
            http: Arc::clone(&self.state),
        }
    }

    /// Advance the state over `data`, one request at a time, so every request
    /// on a keep-alive connection gets its headers processed.
    fn frame_requests(
        &self,
        state: &mut HttpParsingState,
        data: &[u8],
    ) -> Result<Vec<u8>, TcpError> {
        let policy = &*self.policy;
        let (host, port) = (self.host.as_str(), self.port);
        // A connection carrying credentials takes only requests it can
        // frame, check and inject into, as the child proxy's do.
        let http_only = self.http_only || !self.credentials.is_empty();
        let checked = http_only || policy.any_http_rules();
        let mut out = Vec::new();
        let mut input = data.to_vec();
        let mut pos = 0;
        loop {
            match &mut state.body {
                Body::None if state.raw => {
                    // The read side marks a connection raw when the server
                    // speaks first; a host with HTTP rules, or the child
                    // proxy's, still gets nothing it can't check (as in
                    // `classify`).
                    if pos < input.len() && (http_only || policy.has_http_rules(host, port)) {
                        return Err(TcpError::NotPermitted(format!(
                            "only HTTP/1.1 requests may be sent on this connection to {host}, \
                             which spoke first"
                        )));
                    }
                    out.extend_from_slice(&input[pos..]);
                    break;
                }
                Body::None if state.switches => {
                    if pos == input.len() {
                        break;
                    }
                    if checked {
                        return Err(TcpError::NotPermitted(
                            "nothing may follow a CONNECT or Upgrade request on this \
                             connection (HTTP rules are set, or it's proxied)"
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
                    let is_request = policy.classify(host, port, state, http_only)?;
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

                    // What follows a CONNECT or a 101 isn't framed or
                    // masked, so a connection carrying a credential takes
                    // neither: the server could send an earlier request's
                    // credential back in pieces.
                    if !self.credentials.is_empty() && switches_protocols(&headers) {
                        return Err(TcpError::NotPermitted(format!(
                            "CONNECT and Upgrade requests (WebSockets, say) can't be sent to \
                             {host}, which a credential is added for"
                        )));
                    }

                    state.body = request_body(&headers)?;
                    state.started = true;
                    state.skipped_crlf = false;
                    state.switches = switches_protocols(&headers);
                    state.requests.push_back(RequestKind::of(&headers));
                    if checked {
                        policy.check_http_rules(&headers, host, port, http_only)?;
                    }
                    let headers = if self.substitute {
                        policy.substitute_http_headers(&headers, host, port)?
                    } else {
                        headers
                    };
                    // After substitution, so no placeholder is ever expanded
                    // inside a credential.
                    let headers = if self.credentials.is_empty() {
                        headers
                    } else {
                        inject_credentials(&headers, &self.credentials, host, port, self.over_tls)?
                    };
                    if self.http_only {
                        out.extend(to_origin_form(&headers, &self.authority));
                    } else {
                        out.extend(headers);
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
}

/// A request's header block as the child proxy forwards it to the origin
/// server at `connected` (`host[:port]`): its target in origin-form (RFC 9112
/// section 3.2.1), with a `Host` header if it had none (from an absolute-form
/// target's authority, else `connected`), and without the headers addressed
/// to the proxy itself (`Proxy-Connection`, `Proxy-Authorization`).
fn to_origin_form(headers: &[u8], connected: &str) -> Vec<u8> {
    // `request_body` has checked that the headers are UTF-8.
    let text = String::from_utf8_lossy(headers);
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut out = String::with_capacity(text.len() + 16);
    let mut authority = connected;
    match request_line.splitn(3, ' ').collect::<Vec<_>>()[..] {
        [method, target, version] if super::is_absolute_form(target) => {
            let rest = target.split_once("://").map_or("", |(_, rest)| rest);
            let (host, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
            let path = path.split('#').next().unwrap_or_default();
            let path = match path {
                "" if method == "OPTIONS" => "*".to_owned(),
                "" => "/".to_owned(),
                query if query.starts_with('?') => format!("/{query}"),
                path => path.to_owned(),
            };
            out.push_str(&format!("{method} {path} {version}\r\n"));
            authority = host;
        }
        _ => {
            out.push_str(request_line);
            out.push_str("\r\n");
        }
    }
    let mut has_host = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("proxy-authorization")
        {
            continue;
        }
        has_host |= name.eq_ignore_ascii_case("host");
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !has_host && !authority.is_empty() {
        out.push_str(&format!("Host: {authority}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// The request head `headers`, on a connection to `host` on `port` (`over_tls`
/// or not), with `credentials` applied: each one for the request replaces
/// every header of its names the request had, and adds all of its own (the
/// first to set a name wins, and one sharing a name with a credential
/// already applied isn't applied at all, so callers put an execution's
/// before the sandbox's), and `Accept-Encoding` asks for `identity`, since a
/// credential can't be masked out of a compressed response. Every request
/// loses its `Range` and `If-Range`, whether it gets a credential or not, so
/// an echo of one (a log of earlier requests, say) can't be fetched in
/// pieces shorter than the masks. A `CONNECT` or `Upgrade` request gets none
/// (the framer refuses them on these connections anyway): what follows it
/// isn't framed or masked.
///
/// # Errors
///
/// Refuses a request a credential applies to that it can't be sent over
/// (plain HTTP without [`Credential::allow_plaintext`]), rather than send it
/// without.
fn inject_credentials(
    headers: &[u8],
    credentials: &[Arc<Credential>],
    host: &str,
    port: u16,
    over_tls: bool,
) -> Result<Vec<u8>, TcpError> {
    // `request_body` has checked that the headers are UTF-8, and the
    // request line parses (`classify`).
    let text = String::from_utf8_lossy(headers);
    let (method, target) = request_line(&text).unwrap_or_default();
    let mut applied: Vec<&Credential> = Vec::new();
    // Every credential the request matches strips its headers, applied or
    // not, so no copy of them gets through.
    let mut matched: Vec<&Credential> = Vec::new();
    if !switches_protocols(headers) {
        for credential in credentials
            .iter()
            .filter(|c| c.for_request(host, port, method, target))
        {
            matched.push(credential);
            if applied.iter().any(|a| a.overlaps(credential)) {
                continue;
            }
            if !credential.sends_over(over_tls) {
                return Err(TcpError::NotPermitted(format!(
                    "a credential for {host} is only sent over TLS: use https"
                )));
            }
            applied.push(credential);
        }
    }
    let mut lines = text.lines();
    let mut out = String::new();
    if let Some(request_line) = lines.next() {
        out.push_str(request_line);
        out.push_str("\r\n");
    }
    for line in lines.filter(|line| !line.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim();
        let ranged = name.eq_ignore_ascii_case("range") || name.eq_ignore_ascii_case("if-range");
        if name.eq_ignore_ascii_case("accept-encoding")
            || ranged
            || applied.iter().any(|c| c.sets(name))
            || matched.iter().any(|c| c.strips(name))
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    for credential in applied {
        out.push_str(&credential.header_lines());
    }
    out.push_str("Accept-Encoding: identity\r\n\r\n");
    Ok(out.into_bytes())
}

/// Frames and masks the responses read from one connection, by the requests
/// its [`RequestFramer`] framed: real secret values are replaced with their
/// placeholders wherever the guest's HTTP client will see them whole (see
/// [`super::response`]).
///
/// Each comes from its connection's framer, [`RequestFramer::response_masker`].
#[derive(Debug)]
pub(crate) struct ResponseMasker {
    /// `None` on a connection whose requests get nothing substituted, whose
    /// responses are passed through as read.
    pub(super) masker: Option<response::ResponseMasker>,
    /// A read error that came with held-back bytes still to deliver: those
    /// go out first, and this on the next read.
    error: Option<std::io::Error>,
    /// The connection's request framing, shared with its framer.
    http: Arc<Mutex<HttpParsingState>>,
}

impl ResponseMasker {
    /// Read up to `len` bytes from `stream`, masked.
    ///
    /// What's returned, and when, depends only on the responses' framing
    /// and on when bytes arrive, not on whether they look like a secret. The
    /// read gives up after `io_timeout` in all, except that held-back bytes
    /// the grace period applies to always wait all of it: `io_timeout` may
    /// come from the guest, which must not be able to shorten it.
    ///
    /// Cancel-safe: everything read is fed to the masker before the next
    /// `await`, so a read dropped part way (by the child proxy's `select!`)
    /// loses nothing.
    pub(crate) async fn read<S: tokio::io::AsyncRead + Unpin>(
        &mut self,
        stream: &mut S,
        len: u32,
        io_timeout: Duration,
    ) -> std::io::Result<Vec<u8>> {
        let want = read_buf_len(len);
        if want == 0 {
            return Ok(Vec::new());
        }
        let Self {
            masker,
            error,
            http,
        } = self;
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
                Some(why) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why));
                }
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
                    let mut http = lock(http);
                    // Decided by the connection's first bytes: if its first
                    // write wasn't a request, or the server spoke before
                    // anything was written, it's another protocol, which
                    // nothing is ever substituted on (and never will be: the
                    // write side stays raw).
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
}

/// The read side as main's tests of it see it: a masker with its own
/// request framing state, which [`read_masked`] swaps in for each read.
#[cfg(test)]
pub(super) type ReadState = ResponseMasker;

#[cfg(test)]
impl ResponseMasker {
    /// A masker for `masks`, framing responses by its own requests.
    pub(super) fn new(masks: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Self {
            masker: response::ResponseMasker::new(masks),
            error: None,
            http: Arc::default(),
        }
    }
}

/// [`ResponseMasker::read`] with `http` as the connection's request framing.
#[cfg(test)]
pub(super) async fn read_masked<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    state: &mut ResponseMasker,
    http: &mut HttpParsingState,
    len: u32,
    io_timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    *lock(&state.http) = std::mem::take(http);
    let result = state.read(stream, len, io_timeout).await;
    *http = std::mem::take(&mut *lock(&state.http));
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Answers every lookup with fixed addresses.
    #[derive(Debug)]
    struct FixedResolver(Vec<IpAddr>);

    #[async_trait::async_trait]
    impl super::super::Resolver for FixedResolver {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    /// No policy, outer or narrowed, is built from a config that doesn't
    /// validate, and the error is the config's own.
    #[test]
    fn policies_refuse_invalid_configs() {
        let invalid = [
            NetConfig::default().allow_host("*."),
            NetConfig::default().with_connect_timeout(Duration::ZERO),
            NetConfig::default().with_io_timeout(Duration::ZERO),
        ];
        for config in invalid {
            let expected = config.validate().unwrap_err();
            let err = NetPolicy::new(config.clone(), HashMap::new()).unwrap_err();
            assert_eq!(err, expected);

            let outer = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
            let err = outer.narrowed(config).unwrap_err();
            assert_eq!(err, expected);
        }
        let outer = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
        assert!(outer.narrowed(NetConfig::default()).is_ok());
    }

    /// A narrowed policy can limit a host to a port, and can't reach a port
    /// the policy it narrows doesn't allow.
    #[test]
    fn narrowed_policies_check_ports_against_both() {
        let outer = Arc::new(
            NetPolicy::new(
                NetConfig::default()
                    .allow_host("api.example.com:443")
                    .allow_host("git.example.com"),
                HashMap::new(),
            )
            .unwrap(),
        );
        let inner = outer
            .narrowed(
                NetConfig::default()
                    .allow_host("api.example.com")
                    .allow_host("git.example.com:443"),
            )
            .unwrap();
        assert!(inner.check_host("api.example.com", 443).is_ok());
        assert!(inner.check_host("api.example.com", 8443).is_err());
        assert!(inner.check_host("git.example.com", 443).is_ok());
        assert!(inner.check_host("git.example.com", 22).is_err());
        assert!(outer.check_host("git.example.com", 22).is_ok());
    }

    /// A mixed answer is narrowed to the allowed addresses, so the connection
    /// can't land on the internal one.
    #[tokio::test]
    async fn test_mixed_dns_answer_keeps_only_allowed_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let config =
            NetConfig::default().with_resolver(FixedResolver(vec![ip("127.0.0.1"), ip("8.8.8.8")]));
        let policy = NetPolicy::new(config, HashMap::new()).unwrap();

        let addrs = policy.resolve_allowed("public.example", 443).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(ip("8.8.8.8"), 443)]);
    }
    /// The child proxy forwards every request on a connection in
    /// origin-form, without the headers addressed to the proxy itself.
    #[test]
    fn proxied_requests_reach_the_origin_in_origin_form() {
        let policy = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
        let frame = |data: &[u8]| {
            let mut framer = policy
                .request_framer("api.test", 443, false)
                .http_only("api.test:8080");
            String::from_utf8(framer.frame(data).unwrap()).unwrap()
        };

        // A keep-alive connection: both requests are rewritten.
        let out = frame(
            b"GET http://api.test:8080/x?y=1 HTTP/1.1\r\nHost: api.test:8080\r\n\
              Proxy-Connection: keep-alive\r\nproxy-authorization: Basic c2VjcmV0\r\n\
              Content-Length: 2\r\n\r\nhi\
              POST http://api.test:8080 HTTP/1.1\r\nHost: api.test:8080\r\n\
              Content-Length: 0\r\n\r\n",
        );
        assert_eq!(
            out,
            "GET /x?y=1 HTTP/1.1\r\nHost: api.test:8080\r\nContent-Length: 2\r\n\r\nhi\
             POST / HTTP/1.1\r\nHost: api.test:8080\r\nContent-Length: 0\r\n\r\n"
        );

        // No Host header: one is made from the target, as RFC 9112 asks, or,
        // for an origin-form request (a keep-alive follow-up, say), from the
        // connection's host and port.
        assert_eq!(
            frame(b"GET http://api.test/ HTTP/1.0\r\n\r\n"),
            "GET / HTTP/1.0\r\nHost: api.test\r\n\r\n"
        );
        assert_eq!(
            frame(
                b"GET http://api.test:8080/a HTTP/1.1\r\nHost: api.test:8080\r\n\r\n\
                  GET /x HTTP/1.0\r\n\r\n"
            ),
            "GET /a HTTP/1.1\r\nHost: api.test:8080\r\n\r\n\
             GET /x HTTP/1.0\r\nHost: api.test:8080\r\n\r\n"
        );
        // Already origin-form, as inside a CONNECT tunnel: unchanged.
        assert_eq!(
            frame(b"GET /x HTTP/1.1\r\nHost: api.test\r\n\r\n"),
            "GET /x HTTP/1.1\r\nHost: api.test\r\n\r\n"
        );
    }

    /// Of two credentials setting one header, the first wins (an
    /// execution's, which callers put first); every request on a connection
    /// carrying one loses `Range` and `If-Range`, even one that gets none.
    #[test]
    fn injection_takes_the_first_credential_and_drops_ranges() {
        let credentials = [
            Arc::new(Credential::bearer("execution").for_hosts(["api.test"])),
            Arc::new(Credential::bearer("sandbox").for_hosts(["api.test"])),
            Arc::new(
                Credential::header("X-Key", "{secret}", "key")
                    .for_hosts(["api.test"])
                    .for_paths(["/keyed/"]),
            ),
        ];
        let inject = |head: &str, credentials: &[Arc<Credential>]| {
            String::from_utf8(
                inject_credentials(head.as_bytes(), credentials, "api.test", 443, true).unwrap(),
            )
            .unwrap()
        };
        let out = inject(
            "GET /keyed/x HTTP/1.1\r\nHost: api.test\r\nRange: bytes=0-3\r\nif-range: \"e\"\r\n\r\n",
            &credentials,
        );
        assert!(out.contains("Authorization: Bearer execution\r\n"), "{out}");
        assert!(!out.contains("sandbox"), "{out}");
        assert!(out.contains("X-Key: key\r\n"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("range"), "{out}");
        // No credential for the path: the ranges still go, so a logged
        // credential can't be read back in pieces on the same connection.
        let path_only = [Arc::clone(&credentials[2])];
        let out = inject(
            "GET /other HTTP/1.1\r\nHost: api.test\r\nRange: bytes=0-3\r\nIf-Range: \"e\"\r\n\r\n",
            &path_only,
        );
        assert!(!out.contains("X-Key"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("range"), "{out}");
    }

    /// A credential's headers go together: each replaces every copy the
    /// request had, whatever its case, and one sharing a header with a
    /// credential already applied (an execution's, say) isn't applied at
    /// all, so a request never carries part of one.
    #[test]
    fn credentials_apply_whole_or_not_at_all() {
        let pair = || {
            Arc::new(
                Credential::headers([("Authorization", "Bearer pair"), ("X-Id-Token", "id")])
                    .for_hosts(["api.test"]),
            )
        };
        let inject = |head: &str, credentials: &[Arc<Credential>]| {
            String::from_utf8(
                inject_credentials(head.as_bytes(), credentials, "api.test", 443, true).unwrap(),
            )
            .unwrap()
        };
        let head = "GET / HTTP/1.1\r\nHost: api.test\r\nauthorization: Bearer guest\r\n\
                    X-ID-TOKEN: guest\r\nx-id-token: again\r\nX-Other: kept\r\n\r\n";
        let out = inject(head, &[pair()]);
        assert_eq!(
            out,
            "GET / HTTP/1.1\r\nHost: api.test\r\nX-Other: kept\r\n\
             Authorization: Bearer pair\r\nX-Id-Token: id\r\nAccept-Encoding: identity\r\n\r\n"
        );
        let first = Arc::new(Credential::bearer("first").for_hosts(["api.test"]));
        let out = inject(head, &[first, pair()]);
        assert!(out.contains("Authorization: Bearer first\r\n"), "{out}");
        assert!(
            !out.contains("pair") && !out.contains("X-Id-Token: id"),
            "{out}"
        );
        // The pair didn't apply, so the request's own copy stays.
        assert!(out.contains("X-ID-TOKEN: guest"), "{out}");

        // A stripped header goes from every request a credential matches,
        // even when another's headers win, and stays elsewhere.
        let token = || {
            Arc::new(
                Credential::header("X-Access-Token", "{secret}", "tok")
                    .for_hosts(["api.test"])
                    .for_paths(["/api/"])
                    .stripping(["X-Grafana-Id"]),
            )
        };
        let head = "GET /api/x HTTP/1.1\r\nHost: api.test\r\nx-grafana-id: guest\r\n\
                    X-Grafana-ID: again\r\nX-Access-Token: guest\r\n\r\n";
        let out = inject(head, &[token()]);
        assert!(!out.to_ascii_lowercase().contains("x-grafana-id"), "{out}");
        assert!(
            out.contains("X-Access-Token: tok\r\n") && !out.contains("guest"),
            "{out}"
        );
        let first = Arc::new(
            Credential::header("X-Access-Token", "{secret}", "first").for_hosts(["api.test"]),
        );
        let out = inject(head, &[first, token()]);
        assert!(out.contains("X-Access-Token: first\r\n"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("x-grafana-id"), "{out}");
        let out = inject(&head.replace("/api/x", "/public"), &[token()]);
        assert!(out.contains("x-grafana-id: guest"), "{out}");
    }

    /// On a connection to a credential's host, the script's `eryx:net`
    /// framer (no `http_only`) refuses what it couldn't inject into or
    /// would send the credential somewhere else: bytes that aren't a
    /// request, a request naming another host, and over plain TCP a request
    /// the credential applies to (others go, without it).
    #[test]
    fn credentialed_connections_fail_closed() {
        let policy = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
        let credentials = [Arc::new(
            Credential::headers([("X-A", "a-value"), ("X-B", "b-value")])
                .for_hosts(["api.test:443"])
                .for_paths(["/api/"]),
        )];
        let frame = |port: u16, over_tls: bool, data: &[u8]| {
            policy
                .request_framer("api.test", port, over_tls)
                .with_credentials(&credentials)
                .frame(data)
                .map(|out| String::from_utf8(out).unwrap())
        };
        let out = frame(443, true, b"GET /api/x HTTP/1.1\r\nHost: api.test\r\n\r\n").unwrap();
        assert!(out.contains("X-A: a-value\r\nX-B: b-value\r\n"), "{out}");
        for (data, why) in [
            (&b"\x16\x03\x01 not http"[..], "only HTTP/1.1"),
            (
                b"GET /api/x HTTP/1.1\r\nHost: evil.test\r\n\r\n",
                "other than",
            ),
            (
                b"GET https://evil.test/api/x HTTP/1.1\r\n\r\n",
                "other than",
            ),
        ] {
            let err = frame(443, true, data).unwrap_err().to_string();
            assert!(err.contains(why), "{why}: {err}");
        }
        let err = frame(443, false, b"GET /api/x HTTP/1.1\r\nHost: api.test\r\n\r\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only sent over TLS"), "{err}");
        let out = frame(
            443,
            false,
            b"GET /public HTTP/1.1\r\nHost: api.test\r\n\r\n",
        )
        .unwrap();
        assert!(!out.contains("a-value"), "{out}");
        // Another port isn't the credential's: nothing added, nothing refused.
        let out = frame(
            8443,
            true,
            b"GET /api/x HTTP/1.1\r\nHost: evil.test\r\n\r\n",
        )
        .unwrap();
        assert!(!out.contains("a-value"), "{out}");
    }

    /// A connection carrying a credential refuses `Upgrade` and `CONNECT`
    /// requests, before or after a credentialed one, so nothing on it is
    /// ever passed through unframed; one without a credential takes them.
    #[test]
    fn credentialed_connections_refuse_upgrades() {
        let policy = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
        let credentials = [Arc::new(
            Credential::bearer("tok")
                .for_hosts(["api.test"])
                .for_paths(["/api/"])
                .allow_plaintext(),
        )];
        let framer = |credentials: &[Arc<Credential>]| {
            policy
                .request_framer("api.test", 443, false)
                .http_only("api.test")
                .with_credentials(credentials)
        };
        let upgrade = &b"GET /ws HTTP/1.1\r\nHost: api.test\r\nConnection: Upgrade\r\n\
                         Upgrade: websocket\r\n\r\n"[..];
        for data in [
            upgrade.to_vec(),
            [
                &b"GET /api/x HTTP/1.1\r\nHost: api.test\r\n\r\n"[..],
                upgrade,
            ]
            .concat(),
            b"GET /api/x HTTP/1.1\r\nHost: api.test\r\nupgrade: h2c\r\n\r\n".to_vec(),
            b"CONNECT api.test:443 HTTP/1.1\r\nHost: api.test:443\r\n\r\n".to_vec(),
        ] {
            let err = framer(&credentials).frame(&data).unwrap_err();
            assert!(
                err.to_string().contains("Upgrade"),
                "{err} for {:?}",
                String::from_utf8_lossy(&data)
            );
        }
        // Another host's credential doesn't apply here: the upgrade goes.
        let elsewhere = [Arc::new(
            Credential::bearer("tok")
                .for_hosts(["other.test"])
                .allow_plaintext(),
        )];
        assert!(framer(&elsewhere).frame(upgrade).is_ok());
        assert!(framer(&[]).frame(upgrade).is_ok());
    }
}
