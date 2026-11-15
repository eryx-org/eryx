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
//! - A [`ResponseMasker`] per connection replaces real secret values read
//!   back with their placeholders.
//!
//! [`ConnectionManager`]: super::ConnectionManager

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::{
    Body, IpNet, MAX_HEADER_LEN, NetConfig, TcpError, TlsError, could_be_http_request,
    embedded_ipv4, find_header_end, host_matches_pattern, is_absolute_form, is_http_request,
    is_http2, read_buf_len, request_authorities, request_body, request_line, request_start,
    same_host, strip_port,
};
use crate::secrets::SecretConfig;

/// A sandbox's network policy: its [`NetConfig`], its secrets, and the TLS
/// roots it trusts.
#[derive(Debug)]
pub(crate) struct NetPolicy {
    pub(super) config: NetConfig,
    pub(super) secrets: HashMap<String, SecretConfig>,
    /// `(real value, placeholder)` of each secret, for masking reads.
    read_masks: Vec<(Vec<u8>, Vec<u8>)>,
    tls_config: Arc<ClientConfig>,
    /// The policy this one narrows (see [`NetPolicy::narrowed`]): every check
    /// must pass it too.
    parent: Option<Arc<NetPolicy>>,
}

impl NetPolicy {
    pub(crate) fn new(config: NetConfig, secrets: HashMap<String, SecretConfig>) -> Self {
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
            read_masks: secrets
                .values()
                .filter(|s| !s.real_value.is_empty())
                .map(|s| {
                    (
                        s.real_value.clone().into_bytes(),
                        s.placeholder.clone().into_bytes(),
                    )
                })
                .collect(),
            secrets,
            tls_config: Arc::new(tls_config),
            parent: None,
        }
    }

    /// A policy that allows only what both `self` and `config` allow, e.g. a
    /// child process's narrower network inside its sandbox's.
    ///
    /// Hosts, resolved addresses and HTTP rules are checked against both,
    /// and the connection limit and timeouts are the smaller of the two.
    /// Secrets, their hosts, plaintext substitution, the resolver and the
    /// TLS roots come from `self`, so a narrowed policy can never reach or
    /// reveal more than `self`.
    pub(crate) fn narrowed(self: &Arc<Self>, config: NetConfig) -> Self {
        Self {
            config,
            secrets: self.secrets.clone(),
            read_masks: self.read_masks.clone(),
            tls_config: Arc::clone(&self.tls_config),
            parent: Some(Arc::clone(self)),
        }
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

    /// Whether any HTTP rule in the chain applies to `host`.
    pub(crate) fn has_http_rules(&self, host: &str) -> bool {
        self.chain().any(|p| p.config.has_http_rules(host))
    }

    /// Whether the chain has any HTTP rules at all.
    fn any_http_rules(&self) -> bool {
        self.chain().any(|p| !p.config.http_rules.is_empty())
    }

    /// Check `host` against `blocked_hosts`/`allowed_hosts`.
    ///
    /// Passing doesn't make a host reachable: [`NetPolicy::connect`] checks
    /// it again, along with the addresses it resolves to.
    pub(crate) fn check_host(&self, host: &str) -> Result<(), TcpError> {
        // Check blocked first (always enforced).
        for pattern in &self.config.blocked_hosts {
            if host_matches_pattern(host, pattern) {
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
                .any(|p| host_matches_pattern(host, p))
        {
            return Err(TcpError::NotPermitted(format!(
                "host '{host}' not in allowed list"
            )));
        }

        match &self.parent {
            Some(parent) => parent.check_host(host),
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

    /// Whether `host` may connect to `ip`: an internal host may reach any
    /// address, others only allowed ones, in every policy of the chain.
    fn address_allowed(&self, host: &str, ip: IpAddr) -> bool {
        self.chain().all(|p| {
            p.config
                .internal_hosts
                .iter()
                .any(|pattern| host_matches_pattern(host, pattern))
                || p.is_ip_allowed(ip)
        })
    }

    /// Resolve `host` and keep the addresses the policy allows.
    ///
    /// Only [`NetPolicy::connect`] calls this, and it dials only the returned
    /// addresses, never re-resolving: the check is only as good as the
    /// address actually used.
    async fn resolve_allowed(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, TcpError> {
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

        let (allowed, blocked): (Vec<_>, Vec<_>) = addrs
            .into_iter()
            .partition(|a| self.address_allowed(host, a.ip()));
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
        self.check_host(host)?;
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

    /// A framer for the requests written to a connection to `host`, sent
    /// `over_tls` or in plaintext.
    pub(crate) fn request_framer(self: &Arc<Self>, host: &str, over_tls: bool) -> RequestFramer {
        RequestFramer {
            policy: Arc::clone(self),
            host: host.to_string(),
            substitute: self.substitutes_secrets(over_tls),
            http_only: false,
            state: HttpParsingState::default(),
        }
    }

    /// A masker for the responses read from one connection.
    pub(crate) fn response_masker(self: &Arc<Self>) -> ResponseMasker {
        ResponseMasker {
            policy: Arc::clone(self),
            state: ReadState::default(),
        }
    }

    /// With HTTP rules, whether the buffered bytes start a request (`None`
    /// until that's known).
    ///
    /// Anything a server could take for a request must be framed and
    /// checked, so other bytes are only passed through if they're a
    /// connection's first and its host has no rules; the connection then
    /// stays unparsed. Otherwise they're refused, as is a leading empty line
    /// (which servers skip before a request).
    fn classify_ruled(
        &self,
        host: &str,
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
            Some(false) if state.started || self.has_http_rules(host) => {
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
    /// for the connected `host` and every host the request names. With
    /// `http_only`, the request may only name the connected host.
    fn check_http_rules(
        &self,
        headers: &[u8],
        host: &str,
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
        for host in std::iter::once(host).chain(named.into_iter().map(strip_port)) {
            for policy in self.chain() {
                policy
                    .config
                    .check_http_request(host, method, target)
                    .map_err(TcpError::NotPermitted)?;
            }
        }
        Ok(())
    }

    /// Substitute secret placeholders in HTTP headers sent to `target_host`.
    pub(super) fn substitute_http_headers(
        &self,
        headers: &[u8],
        target_host: &str,
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
    pub(super) fn substitute_secrets_in_text(
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

            // The secret's hosts are the sandbox's (the root policy's).
            let host_allowed = self
                .root()
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
struct HttpParsingState {
    /// Request bytes buffered until the headers are complete.
    buffer: Vec<u8>,
    /// How much of the current request's body is still to come.
    body: Body,
    /// Whether a request has been framed on this connection yet.
    started: bool,
    /// Whether this connection's first bytes weren't HTTP, so with HTTP rules
    /// everything on it is passed through unparsed (see
    /// [`NetConfig::allow_http`]).
    raw: bool,
    /// Whether a write was refused, after which every write is.
    failed: bool,
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
    /// Whether secret placeholders are substituted.
    substitute: bool,
    /// Whether anything but HTTP/1.1 requests is refused (see
    /// [`RequestFramer::http_only`]).
    http_only: bool,
    state: HttpParsingState,
}

impl RequestFramer {
    /// Refuse anything but HTTP/1.1 requests, from the first byte, as if
    /// every host had HTTP rules. For the child proxy, which forwards nothing
    /// it can't check.
    pub(crate) fn http_only(mut self) -> Self {
        self.http_only = true;
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
        if self.state.failed {
            return Err(TcpError::NotPermitted(
                "an earlier write on this connection was refused".into(),
            ));
        }
        let result = self.frame_requests(data);
        // After an error the buffered bytes are dropped, and the connection
        // refuses further writes: its framing can't be trusted any more.
        if result.is_err() {
            self.state = HttpParsingState {
                failed: true,
                ..HttpParsingState::default()
            };
        }
        result
    }

    /// Advance the state over `data`, one request at a time, so every request
    /// on a keep-alive connection gets its headers processed.
    fn frame_requests(&mut self, data: &[u8]) -> Result<Vec<u8>, TcpError> {
        let policy = &*self.policy;
        let host = self.host.as_str();
        let http_only = self.http_only;
        let checked = http_only || policy.any_http_rules();
        let state = &mut self.state;
        let mut out = Vec::new();
        let mut input = data.to_vec();
        let mut pos = 0;
        loop {
            match &mut state.body {
                Body::None if state.raw => {
                    out.extend_from_slice(&input[pos..]);
                    break;
                }
                Body::None => {
                    // Only the new bytes (plus 3 for a split terminator) can
                    // complete the terminator.
                    let searched = state.buffer.len().saturating_sub(3);
                    state.buffer.extend_from_slice(&input[pos..]);
                    let is_request = if checked {
                        policy.classify_ruled(host, state, http_only)?
                    } else {
                        Some(could_be_http_request(&state.buffer))
                    };
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

                    // Not HTTP - pass through everything
                    if !checked && !is_http_request(&headers) {
                        out.extend_from_slice(&headers);
                        out.extend_from_slice(&input);
                        break;
                    }

                    // Check for HTTP/2 (not supported with secrets)
                    if is_http2(&headers) {
                        return Err(TcpError::NotPermitted(
                            "HTTP/2 is not supported here. Use HTTP/1.1.".into(),
                        ));
                    }

                    state.body = request_body(&headers)?;
                    state.started = true;
                    if checked {
                        policy.check_http_rules(&headers, host, http_only)?;
                    }
                    if self.substitute {
                        out.extend(policy.substitute_http_headers(&headers, host)?);
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
}

/// Read data held back by one connection's [`ResponseMasker`].
#[derive(Debug, Default)]
struct ReadState {
    /// Masked bytes not returned yet.
    ready: Vec<u8>,
    /// Trailing bytes that may be the start of a secret split across reads.
    carry: Vec<u8>,
}

/// How long a read waits for more data before releasing bytes held back as
/// the possible start of a secret.
const CARRY_GRACE: Duration = Duration::from_millis(100);

/// Replaces real secret values read from one connection with their
/// placeholders, including values split across reads.
///
/// Each connection gets its own, from [`NetPolicy::response_masker`].
#[derive(Debug)]
pub(crate) struct ResponseMasker {
    policy: Arc<NetPolicy>,
    state: ReadState,
}

impl ResponseMasker {
    /// Read up to `len` bytes from `stream`, masked. Bytes that may start a
    /// secret are held back for up to [`CARRY_GRACE`] waiting for the rest.
    pub(crate) async fn read<S: tokio::io::AsyncRead + Unpin>(
        &mut self,
        stream: &mut S,
        len: u32,
        io_timeout: Duration,
    ) -> std::io::Result<Vec<u8>> {
        let masks = &self.policy.read_masks;
        let state = &mut self.state;
        let want = read_buf_len(len);
        if want == 0 {
            return Ok(Vec::new());
        }
        while state.ready.is_empty() {
            let timeout = if state.carry.is_empty() {
                io_timeout
            } else {
                CARRY_GRACE.min(io_timeout)
            };
            let mut buf = vec![0u8; want];
            let n = match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
                Ok(n) => n?,
                // Nothing followed: the held-back bytes weren't a secret after all.
                Err(_) if !state.carry.is_empty() => 0,
                Err(_) => return Err(std::io::ErrorKind::TimedOut.into()),
            };
            let mut data = std::mem::take(&mut state.carry);
            data.extend_from_slice(&buf[..n]);
            if n > 0 {
                for (real, placeholder) in masks {
                    data = crate::secrets::replace_bytes(&data, real, placeholder);
                }
                let keep = partial_secret_len(&data, masks);
                state.carry = data.split_off(data.len() - keep);
            }
            state.ready = data;
            if n == 0 {
                break;
            }
        }
        let rest = state.ready.split_off(want.min(state.ready.len()));
        Ok(std::mem::replace(&mut state.ready, rest))
    }
}

/// Length of the longest suffix of `data` that is a proper prefix of a
/// secret value in `masks`.
fn partial_secret_len(data: &[u8], masks: &[(Vec<u8>, Vec<u8>)]) -> usize {
    let longest = masks.iter().map(|(real, _)| real.len()).max().unwrap_or(0);
    (1..longest.min(data.len() + 1))
        .rev()
        .find(|&k| {
            masks
                .iter()
                .any(|(real, _)| real.len() > k && data.ends_with(&real[..k]))
        })
        .unwrap_or(0)
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

    /// A mixed answer is narrowed to the allowed addresses, so the connection
    /// can't land on the internal one.
    #[tokio::test]
    async fn test_mixed_dns_answer_keeps_only_allowed_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let config =
            NetConfig::default().with_resolver(FixedResolver(vec![ip("127.0.0.1"), ip("8.8.8.8")]));
        let policy = NetPolicy::new(config, HashMap::new());

        let addrs = policy.resolve_allowed("public.example", 443).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(ip("8.8.8.8"), 443)]);
    }
}
