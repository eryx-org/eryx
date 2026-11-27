//! A host-side forward proxy for child processes' HTTP(S) traffic (#812).
//!
//! Child components bring their own TLS, so the host can't see their requests
//! the way it sees the guest's through `eryx:net`. Instead they get
//! `HTTPS_PROXY`/`HTTP_PROXY` pointing at a [`ChildProxy`], which enforces
//! the sandbox's [`NetPolicy`] exactly as `eryx:net` does:
//!
//! - `CONNECT host:port` is checked against the host rules, then dialled with
//!   [`NetPolicy::connect`] (resolved-address checks, the resolver) and
//!   [`NetPolicy::tls_connect`] (the real certificate). The proxy then
//!   terminates the child's TLS with a leaf certificate for `host` from a
//!   per-proxy CA, advertising only `http/1.1`.
//! - Requests from the child go through a [`RequestFramer`] in HTTP-only
//!   mode: strict framing, HTTP rules, secret substitution, and only the
//!   connected host named. Anything that isn't an HTTP/1.1 request is
//!   refused. Responses go through a [`ResponseMasker`], so the child never
//!   sees a real secret value.
//! - A plain `GET http://host/...` (`HTTP_PROXY`) is handled the same way,
//!   with secrets substituted only if the policy allows plaintext secrets.
//! - With tunnels allowed, `CONNECT` is a blind tunnel to a host the policy
//!   allows instead: no inspection and no secret substitution, but the host
//!   and address checks still apply. Hosts with HTTP rules never get one.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::credential::Credential;
use super::policy::{NetPolicy, RequestFramer, ResponseMasker};
use super::{
    MAX_HEADER_LEN, NetBudget, NetConfig, TcpError, TlsError, find_header_end, request_body,
    request_line,
};
use crate::secrets::SecretConfig;

/// How a sandbox's child processes may reach the network.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub enum ChildNetwork {
    /// Not at all (the default).
    #[default]
    None,
    /// Through the proxy, under the sandbox's own network policy.
    Inherit,
    /// Through the proxy, under only what both the sandbox's policy and this
    /// one allow: a child never gets more than its sandbox. The connection
    /// limit and timeouts are the smaller of the two; secrets, the resolver
    /// and the TLS roots are the sandbox's.
    Narrow(Box<NetConfig>),
}

/// How long the proxy's CA and leaf certificates are valid for, either side
/// of now. They only need to outlive one sandbox.
const CERT_LIFETIME: time::Duration = time::Duration::hours(24);

/// Most hosts whose leaf certificates are kept: CONNECTing to many names
/// mustn't grow host memory without bound.
const MAX_CACHED_HOSTS: usize = 256;

/// Size of each read from either side of a proxied connection.
const BUF_LEN: usize = 16 * 1024;

/// Longest the proxy waits for an upstream's TLS handshake on a `CONNECT`
/// (or less, with a shorter connect timeout). A server that isn't TLS, such
/// as plain HTTP waiting for a request line, may never answer the
/// ClientHello, and the child should hear so well before its execution
/// times out.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// An execution's network for its children: its policy (narrowed, for
/// [`ChildNetwork::Narrow`]) and secrets, and its budget of connections and
/// bytes, which the guest's [`ConnectionManager`](super::ConnectionManager) and every
/// child proxy started from this draw on together. Make one with
/// [`ConnectionManager::child_network`](super::ConnectionManager::child_network).
#[derive(Clone, Debug)]
pub struct ChildNetPolicy {
    policy: Arc<NetPolicy>,
    /// Every budget a connection through it draws on, narrowest first: its
    /// own (the narrowed policy's `max_connections` and `max_bytes`, across
    /// all the proxies started from it), those of the policies it narrows,
    /// and last the execution's, shared with the guest.
    budgets: Vec<NetBudget>,
    /// What the passwords of the proxies started from it (and from the
    /// policies narrowed from it) are derived from.
    key: ProxyKey,
}

/// A random key, one per execution's child network, from which each proxy
/// holding credentials derives its password (see
/// [`ChildNetPolicy::proxy_userinfo`]): so the output can be scrubbed of a
/// password before its proxy starts, and one proxy's password tells nothing
/// about another's.
#[derive(Clone)]
struct ProxyKey(Arc<[u8; 32]>);

impl std::fmt::Debug for ProxyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProxyKey([REDACTED])")
    }
}

/// HMAC-SHA256 (RFC 2104) of `message` under `key`, of at most a block.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    debug_assert!(key.len() <= 64);
    let pad = |byte: u8| {
        let mut block = [byte; 64];
        for (b, k) in block.iter_mut().zip(key) {
            *b ^= k;
        }
        block
    };
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

impl ChildNetPolicy {
    /// `None` for [`ChildNetwork::None`].
    ///
    /// # Errors
    ///
    /// Fails if a [`ChildNetwork::Narrow`] config is invalid (see
    /// [`NetConfig::validate`]), naming it the child network.
    pub(crate) fn new(
        sandbox: Arc<NetPolicy>,
        budget: NetBudget,
        network: ChildNetwork,
    ) -> Result<Option<Self>, String> {
        let policy = match network {
            ChildNetwork::None => return Ok(None),
            ChildNetwork::Inherit => sandbox,
            ChildNetwork::Narrow(config) => Arc::new(
                sandbox
                    .narrowed(*config)
                    .map_err(|e| format!("child network: {e}"))?,
            ),
        };
        let children = NetBudget::new(policy.max_connections(), policy.max_bytes());
        Ok(Some(Self {
            policy,
            budgets: vec![children, budget],
            key: ProxyKey(Arc::new(rand::random())),
        }))
    }

    /// Only what both this and `config` allow, for some of the children
    /// (one command's): its connections and bytes count against `config`'s
    /// limits and every one of this policy's too.
    ///
    /// # Errors
    ///
    /// Fails, as [`NetConfig::validate`] does, if `config` is invalid.
    pub(crate) fn narrowed(&self, config: NetConfig) -> Result<Self, String> {
        let policy = Arc::new(self.policy.narrowed(config)?);
        let own = NetBudget::new(policy.max_connections(), policy.max_bytes());
        Ok(Self {
            policy,
            budgets: std::iter::once(own)
                .chain(self.budgets.iter().cloned())
                .collect(),
            key: self.key.clone(),
        })
    }

    /// The `user:password` the proxy holding credentials for the command
    /// `name` requires from its clients, derived from this execution's key.
    /// The password is lowercase hex, so a failure message cut short
    /// (`placeholder_safe_cut`, which keeps runs of letters and digits
    /// whole) never ends in part of it.
    pub(crate) fn proxy_userinfo(&self, name: &str) -> String {
        use std::fmt::Write as _;
        let password = hmac_sha256(&*self.key.0, name.as_bytes());
        password.iter().fold(String::from("eryx:"), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    /// What output and traces are scrubbed of for the proxy holding
    /// credentials for the command `name`: its `user:password` (so its URL
    /// in `HTTPS_PROXY` reads `http://[REDACTED]@127.0.0.1:<port>`), that as
    /// `Proxy-Authorization: Basic` encodes it, and the password alone.
    pub(crate) fn proxy_sensitive_values(&self, name: &str) -> [String; 3] {
        use base64::Engine as _;
        let userinfo = self.proxy_userinfo(name);
        let encoded = base64::prelude::BASE64_STANDARD.encode(&userinfo);
        let password = userinfo
            .split_once(':')
            .map_or_else(String::new, |(_, password)| password.to_owned());
        [userinfo, encoded, password]
    }

    /// Start a proxy for children that may (`allow_tunnels`) or may not
    /// open blind tunnels. Must be called within a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Fails if the CA can't be generated or the listener can't be bound.
    pub fn start_proxy(&self, allow_tunnels: bool) -> std::io::Result<ChildProxy> {
        ChildProxy::start_with(self, allow_tunnels, &[], "")
    }

    /// [`start_proxy`](Self::start_proxy), adding `credentials` to the
    /// requests of the command `name`'s children. A proxy with credentials
    /// requires the password in [`ChildProxy::userinfo`] (see
    /// [`Self::proxy_userinfo`]) from its clients.
    pub(crate) fn start_proxy_with(
        &self,
        allow_tunnels: bool,
        credentials: &[Arc<Credential>],
        name: &str,
    ) -> std::io::Result<ChildProxy> {
        ChildProxy::start_with(self, allow_tunnels, credentials, name)
    }
}

/// The ports of every child proxy listening in this process. No policy's
/// connections may reach one (see [`is_child_proxy`]): a proxy is only for
/// the children it was started for, and one with credentials would hand
/// them to whoever connected.
static PROXY_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Whether a connection to `addr` may reach a child proxy of this process:
/// its port, on any IPv4 loopback address (the proxies listen on
/// `127.0.0.1`) or `0.0.0.0` (which the kernel routes there), or either as
/// IPv4-mapped IPv6.
pub(super) fn is_child_proxy(addr: SocketAddr) -> bool {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4,
            None => return false,
        },
    };
    (ip.is_loopback() || ip.is_unspecified())
        && PROXY_PORTS
            .lock()
            // A poisoned lock still refuses: the list is only pushed to and
            // removed from, so it's never left half-updated.
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&addr.port())
}

/// A forward proxy for one sandbox's child processes, listening on
/// `127.0.0.1`. It stops when dropped, closing every connection.
pub struct ChildProxy {
    addr: SocketAddr,
    ca_pem: String,
    /// `user:password` its clients must send, if it has credentials.
    userinfo: Option<String>,
    #[cfg(test)]
    ca_der: rustls::pki_types::CertificateDer<'static>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for ChildProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password.
        f.debug_struct("ChildProxy")
            .field("addr", &self.addr)
            .field("userinfo", &self.userinfo.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl ChildProxy {
    /// Start a proxy enforcing `config` (narrowed by `network`) and
    /// substituting `secrets`. `None` for [`ChildNetwork::None`].
    ///
    /// With `allow_tunnels`, **every** `CONNECT` opens a blind tunnel instead
    /// of terminating TLS: no secret substitution, no HTTP rules (hosts with
    /// rules are refused a tunnel), no response scrubbing. Only the
    /// `CONNECT` host is checked, so a child can reach other virtual hosts
    /// served from the same address (e.g. behind a CDN). A tunnel closes
    /// after the I/O timeout with no traffic either way.
    ///
    /// The CA is valid for 24 hours either side of its creation, so the
    /// proxy must not outlive that: start one per execution (or per a
    /// similarly bounded lifetime).
    ///
    /// Must be called within a Tokio runtime, which runs the proxy.
    ///
    /// # Errors
    ///
    /// Fails if the CA can't be generated or the listener can't be bound, or
    /// with [`std::io::ErrorKind::InvalidInput`] if `config` or a narrowed
    /// `network` is invalid (see [`NetConfig::validate`]).
    pub fn start(
        config: NetConfig,
        secrets: HashMap<String, SecretConfig>,
        network: ChildNetwork,
        allow_tunnels: bool,
    ) -> std::io::Result<Option<Self>> {
        let budget = NetBudget::new(config.max_connections(), config.max_bytes());
        let invalid = |e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
        let sandbox = Arc::new(NetPolicy::new(config, secrets).map_err(invalid)?);
        ChildNetPolicy::new(sandbox, budget, network)
            .map_err(invalid)?
            .map(|policy| policy.start_proxy(allow_tunnels))
            .transpose()
    }

    fn start_with(
        net: &ChildNetPolicy,
        allow_tunnels: bool,
        credentials: &[Arc<Credential>],
        name: &str,
    ) -> std::io::Result<Self> {
        let policy = Arc::clone(&net.policy);
        let ca = Ca::new().map_err(std::io::Error::other)?;
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let addr = listener.local_addr()?;
        // Registered even if the lock is poisoned, so it's still refused.
        PROXY_PORTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(addr.port());
        // Children reach only their own proxy, but other processes on the
        // host may reach any: a proxy with credentials wants a password.
        let userinfo = (!credentials.is_empty()).then(|| net.proxy_userinfo(name));
        let proxy_auth = userinfo.as_ref().map(|userinfo| {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::prelude::BASE64_STANDARD.encode(userinfo)
            )
        });
        let ca_pem = ca.pem.clone();
        #[cfg(test)]
        let ca_der = ca.der.clone();
        let shared = Arc::new(Shared {
            connect_timeout: policy.connect_timeout(),
            io_timeout: policy.io_timeout(),
            policy,
            ca,
            allow_tunnels,
            budgets: net.budgets.clone(),
            credentials: credentials.to_vec(),
            proxy_auth,
        });
        let task = tokio::spawn(accept_loop(listener, shared));
        Ok(Self {
            addr,
            ca_pem,
            userinfo,
            #[cfg(test)]
            ca_der,
            task,
        })
    }

    /// The address the proxy listens on (`127.0.0.1:<port>`).
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy's CA certificate (PEM), which children must trust. Its
    /// private key never leaves the host: there's no way to get it.
    #[must_use]
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// The `user:password` its clients must put in its URL
    /// (`http://user:password@127.0.0.1:<port>`), if it has credentials.
    pub(crate) fn userinfo(&self) -> Option<&str> {
        self.userinfo.as_deref()
    }
}

impl Drop for ChildProxy {
    fn drop(&mut self) {
        // The accept loop owns every connection's task, so they stop too.
        self.task.abort();
        let mut ports = PROXY_PORTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = ports.iter().position(|&p| p == self.addr.port()) {
            ports.swap_remove(i);
        }
    }
}

/// The per-proxy CA, and the TLS config for each host it has signed for.
struct Ca {
    issuer: Issuer<'static, KeyPair>,
    pem: String,
    #[cfg(test)]
    der: rustls::pki_types::CertificateDer<'static>,
    hosts: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl Ca {
    fn new() -> Result<Self, rcgen::Error> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "eryx child proxy CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        set_validity(&mut params);
        let cert = params.self_signed(&key)?;
        Ok(Self {
            pem: cert.pem(),
            #[cfg(test)]
            der: cert.der().clone(),
            issuer: Issuer::new(params, key),
            hosts: Mutex::new(HashMap::new()),
        })
    }

    /// A server config presenting a certificate for `host`, ALPN
    /// `http/1.1` only.
    fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>, String> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(config) = hosts.get(host) {
            return Ok(Arc::clone(config));
        }
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let mut params =
            CertificateParams::new(vec![host.to_string()]).map_err(|e| e.to_string())?;
        set_validity(&mut params);
        let cert = params
            .signed_by(&key, &self.issuer)
            .map_err(|e| e.to_string())?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .map_err(|e| e.to_string())?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        // ponytail: drop the whole cache when full; an LRU if re-minting shows up.
        if hosts.len() >= MAX_CACHED_HOSTS {
            hosts.clear();
        }
        hosts.insert(host.to_string(), Arc::clone(&config));
        Ok(config)
    }
}

fn set_validity(params: &mut CertificateParams) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - CERT_LIFETIME;
    params.not_after = now + CERT_LIFETIME;
}

struct Shared {
    policy: Arc<NetPolicy>,
    ca: Ca,
    allow_tunnels: bool,
    connect_timeout: Duration,
    io_timeout: Duration,
    /// The budgets each connection draws on, narrowest first (see
    /// [`ChildNetPolicy::budgets`]); the last is the execution's, shared
    /// with its guest.
    budgets: Vec<NetBudget>,
    /// Credentials added to the children's requests.
    credentials: Vec<Arc<Credential>>,
    /// The `Proxy-Authorization` every request to the proxy must carry, if
    /// it has credentials.
    proxy_auth: Option<String>,
}

impl Shared {
    /// How much one read may take: a buffer, or what's left of the byte
    /// limits (`0` once they're spent).
    fn read_cap(&self) -> usize {
        let left = self
            .budgets
            .iter()
            .filter_map(|b| b.bytes().remaining())
            .min();
        left.map_or(BUF_LEN, |left| {
            BUF_LEN.min(usize::try_from(left).unwrap_or(usize::MAX))
        })
    }

    /// Charge all `n` bytes a child is about to send, or none if they don't
    /// fit.
    fn take_sent(&self, n: usize) -> bool {
        let n = n as u64;
        for (i, budget) in self.budgets.iter().enumerate() {
            if !budget.bytes().take_sent(n) {
                for taken in &self.budgets[..i] {
                    taken.bytes().refund_sent(n);
                }
                return false;
            }
        }
        true
    }

    /// Give back `n` bytes charged by [`take_sent`](Self::take_sent) that
    /// never reached the upstream.
    fn refund_sent(&self, n: usize) {
        let n = n as u64;
        for budget in &self.budgets {
            budget.bytes().refund_sent(n);
        }
    }

    /// Charge up to `n` bytes a child received, returning how many fit.
    fn take_received(&self, n: usize) -> usize {
        let granted = self.budgets.iter().fold(n as u64, |granted, budget| {
            budget.bytes().take_received(granted)
        });
        usize::try_from(granted).unwrap_or(usize::MAX)
    }

    fn over_limit(&self) -> ProxyError {
        let max = self
            .budgets
            .iter()
            .filter_map(|b| b.bytes().max())
            .min()
            .unwrap_or_default();
        ProxyError::Refused(format!(
            "network limit reached: {max} bytes sent and received"
        ))
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else { continue };
                let shared = Arc::clone(&shared);
                tasks.spawn(async move {
                    if let Err(e) = serve(&shared, client).await {
                        tracing::debug!(error = %e, "child proxy connection ended");
                    }
                });
            }
            // Reap finished connections so the set doesn't grow.
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

/// Why a proxied connection was refused or failed.
#[derive(Debug)]
enum ProxyError {
    /// The request isn't one the proxy accepts (400).
    BadRequest(String),
    /// The policy refused it (403).
    Refused(String),
    /// It didn't carry the proxy's password (407).
    AuthRequired,
    /// Too many connections (503).
    Busy,
    /// Reaching the upstream failed (502).
    Upstream(String),
    /// The child's side failed; nothing to reply.
    Io(std::io::Error),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Refused(m) | Self::Upstream(m) => f.write_str(m),
            Self::Busy => f.write_str("too many proxied connections"),
            Self::AuthRequired => f.write_str("this proxy needs the password in its URL"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<TcpError> for ProxyError {
    fn from(e: TcpError) -> Self {
        match e {
            TcpError::NotPermitted(m) => Self::Refused(m),
            other => Self::Upstream(other.to_string()),
        }
    }
}

impl From<TlsError> for ProxyError {
    fn from(e: TlsError) -> Self {
        match e {
            TlsError::Tcp(e) => e.into(),
            other => Self::Upstream(other.to_string()),
        }
    }
}

impl ProxyError {
    /// The response telling the child why, if it can still be sent. The
    /// reason never includes request headers, and any secret placeholder or
    /// value in it (e.g. from a host name or path) is redacted.
    fn response(&self, policy: &NetPolicy) -> Option<Vec<u8>> {
        let status = match self {
            Self::BadRequest(_) => "400 Bad Request",
            Self::Refused(_) => "403 Forbidden",
            Self::AuthRequired => {
                "407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"eryx\""
            }
            Self::Busy => "503 Service Unavailable",
            Self::Upstream(_) => "502 Bad Gateway",
            Self::Io(_) => return None,
        };
        let mut body =
            crate::secrets::scrub_placeholders(&format!("eryx proxy: {self}\n"), &policy.secrets);
        for secret in policy.secrets.values().filter(|s| !s.real_value.is_empty()) {
            body = body.replace(&secret.real_value, "[REDACTED]");
        }
        Some(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
    }
}

/// Serve one connection from a child, replying with the error if it's
/// refused before any response has started.
async fn serve(shared: &Shared, mut client: TcpStream) -> Result<(), ProxyError> {
    // All held for as long as the connection is open.
    let permits = shared
        .budgets
        .iter()
        .map(NetBudget::try_acquire)
        .collect::<Result<Vec<_>, _>>();
    let result = match permits {
        Err(_) => Err(ProxyError::Busy),
        Ok(_permits) => route(shared, &mut client).await,
    };
    if let Err(e) = &result
        && let Some(response) = e.response(&shared.policy)
    {
        let _ = client.write_all(&response).await;
    }
    result
}

/// Read the first request's head and handle it as `CONNECT` or plain HTTP.
/// Errors returned here are reported to the child in plain HTTP.
async fn route(shared: &Shared, client: &mut TcpStream) -> Result<(), ProxyError> {
    let (head, rest) = tokio::time::timeout(shared.io_timeout, read_head(client))
        .await
        .map_err(|_| ProxyError::BadRequest("timed out reading the request".into()))??;
    let text = std::str::from_utf8(&head)
        .map_err(|_| ProxyError::BadRequest("request is not UTF-8".into()))?;
    let (method, target) = request_line(text)
        .ok_or_else(|| ProxyError::BadRequest("malformed request line".into()))?;
    // The framer checks this again, but a head it would refuse (bad framing,
    // a `Host` that isn't a plain authority) gets a 400 now, before anything
    // is dialled.
    request_body(&head).map_err(|e| match e {
        TcpError::NotPermitted(m) => ProxyError::BadRequest(m),
        other => other.into(),
    })?;
    if let Some(expected) = &shared.proxy_auth
        && !proxy_authorized(text, expected)
    {
        return Err(ProxyError::AuthRequired);
    }
    // A spent byte limit gets its 403 now, before anything is dialled,
    // rather than a `200` to a CONNECT tunnel that closes at once.
    if shared.read_cap() == 0 {
        return Err(shared.over_limit());
    }

    if method == "CONNECT" {
        let (host, port) = split_authority(target)
            .ok_or_else(|| ProxyError::BadRequest("bad CONNECT target".into()))?;
        if !rest.is_empty() {
            return Err(ProxyError::BadRequest(
                "data sent before the CONNECT response".into(),
            ));
        }
        return connect(shared, client, host, port).await;
    }

    let authority = target
        .strip_prefix("http://")
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .ok_or_else(|| {
            ProxyError::BadRequest("only CONNECT and absolute http:// requests are proxied".into())
        })?;
    let (host, port) = match split_authority(authority) {
        Some(hp) => hp,
        None if !authority.is_empty() && !authority.contains(':') => (authority, 80),
        None => {
            return Err(ProxyError::BadRequest("bad request target".into()));
        }
    };
    let upstream = shared
        .policy
        .connect(host, port, shared.connect_timeout)
        .await?
        .into_stream();
    let framer = shared
        .policy
        .request_framer(host, port, false)
        .http_only(authority)
        .with_credentials(&shared.credentials);
    let mut first = head;
    first.extend_from_slice(&rest);
    pump(shared, client, upstream, framer, first).await
}

/// Handle `CONNECT host:port`: a checked upstream, then either a blind
/// tunnel or the child's TLS terminated and its requests proxied.
async fn connect(
    shared: &Shared,
    client: &mut TcpStream,
    host: &str,
    port: u16,
) -> Result<(), ProxyError> {
    let policy = &shared.policy;
    let upstream = policy.connect(host, port, shared.connect_timeout).await?;

    if shared.allow_tunnels {
        if policy.has_http_rules(host, port) {
            return Err(ProxyError::Refused(format!(
                "{host} has HTTP rules, so it can't be tunnelled"
            )));
        }
        if shared.credentials.iter().any(|c| c.for_host(host, port)) {
            return Err(ProxyError::Refused(format!(
                "{host} has a credential, so it can't be tunnelled"
            )));
        }
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
        splice(shared, client, upstream.into_stream()).await;
        return Ok(());
    }

    let handshake = shared.connect_timeout.min(TLS_HANDSHAKE_TIMEOUT);
    let not_tls = "CONNECT is for TLS; if it serves plain HTTP, send the http:// URL to \
                   HTTP_PROXY instead";
    let upstream = policy
        .tls_connect(upstream, host, handshake)
        .await
        .map_err(|e| match e {
            TlsError::Tcp(TcpError::TimedOut) => ProxyError::Upstream(format!(
                "TLS handshake with {host}:{port} timed out after {handshake:?} ({not_tls})"
            )),
            // rustls reports bytes that aren't TLS records, such as a plain
            // HTTP reply, as a corrupt message; other failures (e.g. an
            // untrusted certificate) are TLS servers, and get no hint.
            TlsError::HandshakeFailed(reason) if reason.contains("corrupt message") => {
                ProxyError::Upstream(format!(
                    "TLS handshake with {host}:{port} failed: {reason} ({not_tls})"
                ))
            }
            TlsError::HandshakeFailed(reason) => {
                ProxyError::Upstream(format!("TLS handshake with {host}:{port} failed: {reason}"))
            }
            other => other.into(),
        })?;
    let config = shared
        .ca
        .server_config(host)
        .map_err(ProxyError::Upstream)?;
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;

    // From here on the child talks TLS, so failures just close it. Anything
    // but a TLS handshake record (0x16) is refused at once, rather than
    // read as a TLS record header and waited on.
    let mut first = [0u8; 1];
    let peeked = tokio::time::timeout(shared.connect_timeout, client.peek(&mut first))
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    if peeked == 0 || first[0] != 0x16 {
        return Err(ProxyError::Io(std::io::Error::other(
            "the CONNECT tunnel didn't start with a TLS handshake",
        )));
    }
    let child = tokio::time::timeout(
        shared.connect_timeout,
        TlsAcceptor::from(config).accept(&mut *client),
    )
    .await
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    if let Some(name) = child.get_ref().1.server_name()
        && !super::same_host(name, host)
    {
        return Err(ProxyError::Io(std::io::Error::other(format!(
            "TLS server name {name} is not the CONNECT host {host}"
        ))));
    }
    let framer = policy
        .request_framer(host, port, true)
        .http_only(host_header(host, port, 443))
        .with_credentials(&shared.credentials);
    pump(shared, child, upstream, framer, Vec::new()).await
}

/// Relay a child connection to its upstream: child bytes through `framer`
/// (starting with `first`), upstream bytes through a [`ResponseMasker`].
/// A refused request gets a `403` and ends the connection.
async fn pump<C, U>(
    shared: &Shared,
    child: C,
    upstream: U,
    mut framer: RequestFramer,
    first: Vec<u8>,
) -> Result<(), ProxyError>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut child_r, mut child_w) = tokio::io::split(child);
    let (mut up_r, mut up_w) = tokio::io::split(upstream);
    // Paired with the framer, whose requests frame the responses.
    let mut masker: ResponseMasker = framer.response_masker();

    if !first.is_empty() {
        // Nothing has been answered yet, so the child can still get a 403.
        if !shared.take_sent(first.len()) {
            return Err(shared.over_limit());
        }
        // A write that fails may have sent some of it, so only a request
        // refused before anything was written is refunded.
        match framer.frame(&first) {
            Ok(out) => up_w.write_all(&out).await?,
            Err(e) => {
                shared.refund_sent(first.len());
                return Err(e.into());
            }
        }
    }

    let mut buf = vec![0u8; BUF_LEN];
    let mut child_done = false;
    loop {
        // Mid-stream, a spent byte limit just ends the connection.
        let cap = shared.read_cap();
        if cap == 0 {
            let _ = child_w.shutdown().await;
            return Ok(());
        }
        tokio::select! {
            n = child_r.read(&mut buf[..cap]), if !child_done => {
                let n = n?;
                if n == 0 {
                    child_done = true;
                    let _ = up_w.shutdown().await;
                    continue;
                }
                if !shared.take_sent(n) {
                    let _ = child_w.shutdown().await;
                    return Ok(());
                }
                match framer.frame(&buf[..n]) {
                    // Not refunded if it fails: some of it may have gone.
                    Ok(out) => up_w.write_all(&out).await?,
                    Err(e) => {
                        shared.refund_sent(n);
                        if let Some(response) = ProxyError::from(e).response(&shared.policy) {
                            let _ = child_w.write_all(&response).await;
                        }
                        let _ = child_w.shutdown().await;
                        return Ok(());
                    }
                }
            }
            data = masker.read(&mut up_r, cap as u32, shared.io_timeout) => {
                let data = data?;
                if data.is_empty() {
                    let _ = child_w.shutdown().await;
                    return Ok(());
                }
                let granted = shared.take_received(data.len());
                child_w.write_all(&data[..granted]).await?;
                if granted < data.len() {
                    let _ = child_w.shutdown().await;
                    return Ok(());
                }
            }
        }
    }
}

/// Copy bytes both ways between the `child` and its `upstream`, charging
/// them to the byte limits, until either side closes, the limits are spent,
/// or neither sends anything for the I/O timeout.
async fn splice<A, B>(shared: &Shared, child: A, upstream: B)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_r, mut a_w) = tokio::io::split(child);
    let (mut b_r, mut b_w) = tokio::io::split(upstream);
    let mut a_buf = vec![0u8; BUF_LEN];
    let mut b_buf = vec![0u8; BUF_LEN];
    loop {
        let cap = shared.read_cap();
        let step = tokio::time::timeout(shared.io_timeout, async {
            if cap == 0 {
                return false;
            }
            tokio::select! {
                n = a_r.read(&mut a_buf[..cap]) => match n {
                    // Not refunded if the write fails: some of it may
                    // have gone.
                    Ok(n) if n > 0 && shared.take_sent(n) => {
                        b_w.write_all(&a_buf[..n]).await.is_ok()
                    }
                    _ => false,
                },
                n = b_r.read(&mut b_buf[..cap]) => match n {
                    Ok(n) if n > 0 => {
                        let granted = shared.take_received(n);
                        a_w.write_all(&b_buf[..granted]).await.is_ok() && granted == n
                    }
                    _ => false,
                },
            }
        })
        .await;
        if !matches!(step, Ok(true)) {
            let _ = a_w.shutdown().await;
            let _ = b_w.shutdown().await;
            return;
        }
    }
}

/// Read up to the end of a request's head (`\r\n\r\n`), returning it and
/// any bytes read past it.
async fn read_head(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), ProxyError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(ProxyError::BadRequest(
                "connection closed mid-request".into(),
            ));
        }
        let searched = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_header_end(&buf[searched..]) {
            let rest = buf.split_off(searched + end);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEADER_LEN {
            return Err(ProxyError::BadRequest(format!(
                "request headers exceed {MAX_HEADER_LEN} bytes"
            )));
        }
    }
}

/// Whether the request head `text` carries `expected` as its only
/// `Proxy-Authorization`.
fn proxy_authorized(text: &str, expected: &str) -> bool {
    let mut values = text.lines().skip(1).filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim())
    });
    values.next().is_some_and(|value| {
        // Constant-time, so the password can't be guessed byte by byte.
        value.len() == expected.len()
            && value
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }) && values.next().is_none()
}

/// `host` and `port` of a `host:port` or `[v6]:port` authority.
fn split_authority(authority: &str) -> Option<(&str, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = port.parse().ok()?;
    (!host.is_empty() && !host.contains(':') || authority.starts_with('[')).then_some((host, port))
}

/// `host` and `port` as a `Host` header value: an IPv6 literal (which
/// [`split_authority`] unbrackets) bracketed, and the port left out when it's
/// the scheme's `default`.
fn host_header(host: &str, port: u16, default: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if port == default {
        host
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::net::Resolver;
    use rustls::pki_types::ServerName;
    use tokio_rustls::TlsConnector;

    /// A child network, a command's narrower one and a standalone proxy are
    /// each refused an invalid config, naming it.
    #[tokio::test]
    async fn child_networks_refuse_invalid_configs() {
        let bad = || NetConfig::default().allow_host("*.");
        let expected = bad().validate().unwrap_err();
        let sandbox = Arc::new(NetPolicy::new(NetConfig::default(), HashMap::new()).unwrap());
        let budget = NetBudget::new(0, None);
        let narrow = ChildNetwork::Narrow(Box::new(bad()));

        let err =
            ChildNetPolicy::new(Arc::clone(&sandbox), budget.clone(), narrow.clone()).unwrap_err();
        assert_eq!(err, format!("child network: {expected}"));

        let children = ChildNetPolicy::new(sandbox, budget, ChildNetwork::Inherit)
            .unwrap()
            .unwrap();
        assert_eq!(children.narrowed(bad()).unwrap_err(), expected);

        let err =
            ChildProxy::start(bad(), HashMap::new(), ChildNetwork::Inherit, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err =
            ChildProxy::start(NetConfig::default(), HashMap::new(), narrow, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().starts_with("child network: "), "{err}");
    }

    #[test]
    fn host_headers_bracket_ipv6_literals() {
        assert_eq!(host_header("::1", 8443, 443), "[::1]:8443");
        assert_eq!(host_header("::1", 443, 443), "[::1]");
        assert_eq!(host_header("api.test", 443, 443), "api.test");
        assert_eq!(host_header("api.test", 8443, 443), "api.test:8443");
    }

    const REAL: &str = "real-secret-value-0123456789";
    const PH: &str = "ERYX_PROXY_TEST_PLACEHOLDER_0123456789";

    /// Resolves every name to 127.0.0.1, where the test upstreams listen.
    #[derive(Debug)]
    struct Localhost;

    #[async_trait::async_trait]
    impl Resolver for Localhost {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        }
    }

    type Received = Arc<Mutex<Vec<u8>>>;

    /// Answer one request with everything received, as the body.
    async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, received: Received) {
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while find_header_end(&head).is_none() {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        received.lock().unwrap().extend_from_slice(&head);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            head.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(&head).await;
        let _ = stream.shutdown().await;
    }

    /// An echoing upstream on 127.0.0.1, over TLS with a certificate for
    /// `api.test` and `other.test` if `tls`. Returns its port, the
    /// certificate (DER) and what it received.
    async fn upstream(tls: bool) -> (u16, Vec<u8>, Received) {
        let cert = rcgen::generate_simple_self_signed(vec!["api.test".into(), "other.test".into()])
            .unwrap();
        let der = cert.cert.der().to_vec();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let acceptor = TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.cert.der().clone()], key)
                .unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Received::default();
        let seen = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(stream).await {
                            echo(stream, seen).await;
                        }
                    } else {
                        echo(stream, seen).await;
                    }
                });
            }
        });
        (port, der, received)
    }

    fn config(cert: &[u8]) -> NetConfig {
        NetConfig::default()
            .allow_host("api.test")
            .allow_host("other.test")
            .allow_internal_host("*.test")
            .with_resolver(Localhost)
            .with_root_cert(cert.to_vec())
    }

    fn secrets() -> HashMap<String, SecretConfig> {
        let secret = SecretConfig {
            placeholder: PH.to_string(),
            ..SecretConfig::new(REAL, vec!["api.test".into()])
        };
        HashMap::from([("KEY".to_string(), secret)])
    }

    fn proxy(config: NetConfig, network: ChildNetwork, tunnels: bool) -> ChildProxy {
        ChildProxy::start(config, secrets(), network, tunnels)
            .unwrap()
            .unwrap()
    }

    /// Send `CONNECT target` and return the stream and the response's status
    /// line.
    async fn connect_to(proxy: &ChildProxy, target: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        stream
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let (head, _) = read_head(&mut stream).await.unwrap();
        let status = String::from_utf8(head).unwrap();
        (stream, status.lines().next().unwrap().to_string())
    }

    /// TLS to `host` through a `CONNECT`ed stream, trusting only the
    /// proxy's CA, offering h2 and http/1.1.
    async fn tls(
        proxy: &ChildProxy,
        stream: TcpStream,
        host: &str,
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(proxy.ca_der.clone()).unwrap();
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from(host.to_string()).unwrap(), stream)
            .await
            .unwrap()
    }

    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, request: &[u8]) -> String {
        stream.write_all(request).await.unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    fn received(received: &Received) -> String {
        String::from_utf8_lossy(&received.lock().unwrap()).into_owned()
    }

    /// A child's HTTPS request reaches the upstream with the real secret,
    /// over http/1.1, and the response it gets back shows the placeholder.
    #[tokio::test]
    async fn https_requests_get_secrets_substituted_and_scrubbed() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        let (stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut tls = tls(&proxy, stream, "api.test").await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));

        let request = format!(
            "GET /x HTTP/1.1\r\nHost: api.test\r\nAuthorization: Bearer {PH}\r\nConnection: close\r\n\r\n"
        );
        let response = exchange(&mut tls, request.as_bytes()).await;
        assert!(
            received(&seen).contains(&format!("Bearer {REAL}")),
            "{}",
            received(&seen)
        );
        assert!(
            response.contains(" 200 OK") && response.contains(PH),
            "{response}"
        );
        assert!(!response.contains(REAL), "{response}");
    }

    /// A `CONNECT` to a server that isn't TLS gets a 502 naming the TLS
    /// handshake: soon if the server answers the ClientHello with plain
    /// HTTP, and within the handshake bound if it waits for a request.
    #[tokio::test]
    async fn plain_http_upstreams_fail_the_tls_handshake_clearly() {
        /// The whole response to `CONNECT api.test:{port}`.
        async fn refusal(proxy: &ChildProxy, port: u16) -> String {
            let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
            let target = format!("api.test:{port}");
            stream
                .write_all(
                    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response).await;
            String::from_utf8_lossy(&response).into_owned()
        }

        // Waits for a request head, which a ClientHello never completes.
        let (port, cert, _) = upstream(false).await;
        let timeout = Duration::from_millis(300);
        let proxy = proxy(
            config(&cert).with_connect_timeout(timeout),
            ChildNetwork::Inherit,
            false,
        );
        let started = std::time::Instant::now();
        let message = refusal(&proxy, port).await;
        assert!(message.starts_with("HTTP/1.1 502 "), "{message}");
        assert!(
            message.contains(&format!(
                "TLS handshake with api.test:{port} timed out after 300ms"
            )) && message.contains("http:// URL to HTTP_PROXY"),
            "{message}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // Answers with a plain HTTP error and closes.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.read(&mut [0u8; 1024]).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        let proxy = super::tests::proxy(config(&cert), ChildNetwork::Inherit, false);
        let started = std::time::Instant::now();
        let message = refusal(&proxy, port).await;
        assert!(message.starts_with("HTTP/1.1 502 "), "{message}");
        assert!(
            message.contains(&format!("TLS handshake with api.test:{port} failed:"))
                && message.contains("http:// URL to HTTP_PROXY"),
            "{message}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // A TLS server with a certificate the proxy doesn't trust gets no
        // plain-HTTP hint.
        let (port, _, _) = upstream(true).await;
        let proxy = super::tests::proxy(config(&[]), ChildNetwork::Inherit, false);
        let message = refusal(&proxy, port).await;
        assert!(
            message.starts_with("HTTP/1.1 502 ")
                && message.contains(&format!("TLS handshake with api.test:{port} failed:"))
                && !message.contains("plain HTTP"),
            "{message}"
        );
    }

    /// Hosts the policy refuses get a 403, before anything is dialled.
    #[tokio::test]
    async fn refused_hosts_get_a_403() {
        let (port, cert, _) = upstream(true).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        let (_, status) = connect_to(&proxy, &format!("evil.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");

        // Allowed by name, but resolving to a blocked network.
        let mut blocked = config(&cert);
        blocked = blocked.with_internal_hosts(Vec::<String>::new());
        let proxy = super::tests::proxy(blocked, ChildNetwork::Inherit, false);
        let (_, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");
    }

    /// Inside a CONNECT, only HTTP/1.1 requests for that host go through.
    #[tokio::test]
    async fn only_http_requests_for_the_connect_host_go_through() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        for request in [
            &b"\x00\x01 not http\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n",
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        ] {
            let (stream, _) = connect_to(&proxy, &format!("api.test:{port}")).await;
            let mut tls = tls(&proxy, stream, "api.test").await;
            let response = exchange(&mut tls, request).await;
            assert!(
                response.starts_with("HTTP/1.1 403 "),
                "{request:?}: {response}"
            );
        }
        assert_eq!(received(&seen), "");
    }

    /// A request head with a `Host` that isn't a plain `host[:port]`, or
    /// chunked on HTTP/1.0, gets a 400 before anything is dialled; inside a
    /// CONNECT the framer refuses the same requests.
    #[tokio::test]
    async fn malformed_hosts_and_framing_get_a_400() {
        let (port, cert, seen) = upstream(false).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        for request in [
            format!("GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test:x@other.test\r\n\r\n"),
            format!("GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test/x\r\n\r\n"),
            format!(
                "POST http://api.test:{port}/ HTTP/1.0\r\nHost: api.test:{port}\r\n\
                 Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            format!("CONNECT api.test:{port} HTTP/1.1\r\nHost: api.test:{port}@x\r\n\r\n"),
        ] {
            let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
            let response = exchange(&mut stream, request.as_bytes()).await;
            assert!(
                response.starts_with("HTTP/1.1 400 "),
                "{request:?}: {response}"
            );
        }
        assert_eq!(received(&seen), "");

        let (port, cert, seen) = upstream(true).await;
        let proxy = super::tests::proxy(config(&cert), ChildNetwork::Inherit, false);
        for request in [
            "GET / HTTP/1.1\r\nHost: api.test:x@other.test\r\n\r\n",
            "POST / HTTP/1.0\r\nHost: api.test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            let (stream, _) = connect_to(&proxy, &format!("api.test:{port}")).await;
            let mut tls = tls(&proxy, stream, "api.test").await;
            let response = exchange(&mut tls, request.as_bytes()).await;
            assert!(
                response.starts_with("HTTP/1.1 403 "),
                "{request:?}: {response}"
            );
        }
        assert_eq!(received(&seen), "");
    }

    /// With tunnels allowed, CONNECT is blind: bytes pass as sent, with no
    /// substitution, but the host checks still apply.
    #[tokio::test]
    async fn tunnels_are_blind_and_opt_in() {
        let (port, cert, seen) = upstream(false).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, true);
        let (mut stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let request = format!("GET / HTTP/1.1\r\nHost: api.test\r\nAuthorization: {PH}\r\n\r\n");
        let response = exchange(&mut stream, request.as_bytes()).await;
        assert!(response.contains(" 200 OK"), "{response}");
        assert!(received(&seen).contains(PH), "{}", received(&seen));

        let (_, status) = connect_to(&proxy, &format!("evil.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");

        // A host with HTTP rules can't be tunnelled: the rules couldn't apply.
        let ruled = config(&cert).allow_http(crate::net::HttpRule::new("api.test"));
        let proxy = super::tests::proxy(ruled, ChildNetwork::Inherit, true);
        let (_, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");
    }

    /// A plain `HTTP_PROXY` request is proxied, but plaintext gets no
    /// secrets unless the policy allows it.
    #[tokio::test]
    async fn plain_http_requests_are_proxied() {
        let (port, cert, seen) = upstream(false).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        let request = format!(
            "GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test:{port}\r\nAuthorization: {PH}\r\nConnection: close\r\n\r\n"
        );
        let response = exchange(&mut stream, request.as_bytes()).await;
        assert!(response.contains(" 200 OK"), "{response}");
        assert!(received(&seen).contains(PH) && !received(&seen).contains(REAL));
    }

    /// A child can't reach a blocked host through another spelling of it:
    /// `CONNECT` targets, absolute-form requests and a narrowed child
    /// network's blocks all match the canonical form, as the resolver does.
    #[tokio::test]
    async fn other_spellings_of_a_blocked_host_are_refused() {
        let (port, cert, _) = upstream(false).await;
        let spellings = [
            "127.0.0.1",
            "127.0.0.1.",
            "2130706433",
            "0x7f.1",
            "0177.0.0.1",
            "127.1",
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
        ];
        // Every name resolves to 127.0.0.1 and may reach it; only the
        // literal `127.0.0.1` is blocked (not the default `127.*`, so the
        // other spellings are refused by matching it, not by a wildcard).
        let open = |cert: &[u8]| {
            NetConfig::default()
                .allow_all_hosts(true)
                .with_blocked_hosts(Vec::<String>::new())
                .allow_internal_host("*")
                .with_resolver(Localhost)
                .with_root_cert(cert.to_vec())
        };
        let blocked = open(&cert).block_host("127.0.0.1");
        let narrowed_block = NetConfig::default()
            .allow_all_hosts(true)
            .with_blocked_hosts(["127.0.0.1"])
            .allow_internal_host("*");
        for proxy in [
            proxy(blocked, ChildNetwork::Inherit, true),
            proxy(
                open(&cert),
                ChildNetwork::Narrow(Box::new(narrowed_block)),
                true,
            ),
        ] {
            let (_, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
            assert!(status.contains(" 200 "), "api.test: {status}");
            for host in spellings {
                let (_, status) = connect_to(&proxy, &format!("{host}:{port}")).await;
                assert!(status.contains(" 403 "), "CONNECT {host}: {status}");

                let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
                let request = format!(
                    "GET http://{host}:{port}/ HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
                );
                let response = exchange(&mut stream, request.as_bytes()).await;
                assert!(
                    response.starts_with("HTTP/1.1 403 "),
                    "GET {host}: {response}"
                );
            }
        }
    }

    /// A proxy for the children of `config`'s sandbox, adding `credentials`.
    fn credential_proxy(
        config: NetConfig,
        credentials: &[Credential],
        tunnels: bool,
    ) -> ChildProxy {
        let budget = NetBudget::new(config.max_connections(), config.max_bytes());
        let sandbox = Arc::new(NetPolicy::new(config, secrets()).unwrap());
        let credentials: Vec<_> = credentials.iter().cloned().map(Arc::new).collect();
        ChildNetPolicy::new(sandbox, budget, ChildNetwork::Inherit)
            .unwrap()
            .unwrap()
            .start_proxy_with(tunnels, &credentials, "cmd")
            .unwrap()
    }

    /// `CONNECT target` with the proxy's password, as its children send it.
    async fn connect_authorized(proxy: &ChildProxy, target: &str) -> (TcpStream, String) {
        use base64::Engine as _;
        let auth = base64::prelude::BASE64_STANDARD.encode(proxy.userinfo().unwrap());
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        stream
            .write_all(
                format!(
                    "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\
                     Proxy-Authorization: Basic {auth}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let (head, _) = read_head(&mut stream).await.unwrap();
        let status = String::from_utf8(head).unwrap();
        (stream, status.lines().next().unwrap().to_string())
    }

    const TOKEN: &str = "test-injected-token-0123456789abcdef";

    /// A credential replaces the header of its name the child sent, asks for
    /// an uncompressed response, and is masked out of what comes back (here
    /// an echo of the whole request, which carries it).
    #[tokio::test]
    async fn credentials_replace_the_childs_header_and_are_masked() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = credential_proxy(
            config(&cert),
            &[Credential::token(TOKEN).for_hosts(["api.test"])],
            false,
        );
        let (stream, status) = connect_authorized(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut tls = tls(&proxy, stream, "api.test").await;
        let response = exchange(
            &mut tls,
            b"GET /user HTTP/1.1\r\nHost: api.test\r\nauthorization: token dummy\r\n\
              Authorization: Bearer other\r\nAccept-Encoding: gzip, br\r\nConnection: close\r\n\r\n",
        )
        .await;
        let seen = received(&seen);
        assert!(
            seen.contains(&format!("Authorization: token {TOKEN}\r\n")),
            "{seen}"
        );
        assert!(
            !seen.contains("dummy") && !seen.contains("Bearer other"),
            "{seen}"
        );
        assert!(seen.contains("Accept-Encoding: identity\r\n") && !seen.contains("gzip"));
        assert!(response.contains(" 200 OK"), "{response}");
        assert!(!response.contains(TOKEN), "{response}");
        assert!(response.contains("Authorization: [REDACTED]"), "{response}");
    }

    /// A proxy with credentials has a password derived from its child
    /// network's key and its command, known before it starts (so output can
    /// be scrubbed of it), different for each command and each execution,
    /// and never in its `Debug`.
    #[tokio::test]
    async fn proxy_passwords_are_derived_and_never_debugged() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );

        let config = NetConfig::default().allow_host("api.test");
        let budget = NetBudget::new(config.max_connections(), config.max_bytes());
        let sandbox = Arc::new(NetPolicy::new(config, secrets()).unwrap());
        let net = ChildNetPolicy::new(Arc::clone(&sandbox), budget.clone(), ChildNetwork::Inherit)
            .unwrap()
            .unwrap();
        let credentials = [Arc::new(Credential::bearer(TOKEN).for_hosts(["api.test"]))];
        let proxy = net.start_proxy_with(false, &credentials, "gh").unwrap();
        let userinfo = proxy.userinfo().unwrap();
        let [scrubbed, encoded, password] = net.proxy_sensitive_values("gh");
        assert_eq!(userinfo, scrubbed);
        assert_eq!(format!("eryx:{password}"), userinfo);
        // 64 lowercase hex digits: no character a failure message's cut
        // (`placeholder_safe_cut`) could split it at.
        assert_eq!(password.len(), 64);
        assert!(
            password
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "the proxy password isn't lowercase hex"
        );
        {
            use base64::Engine as _;
            assert_eq!(encoded, base64::prelude::BASE64_STANDARD.encode(userinfo));
        }
        assert_eq!(
            net.narrowed(NetConfig::default())
                .unwrap()
                .proxy_userinfo("gh"),
            userinfo
        );
        assert_ne!(net.proxy_userinfo("git"), userinfo);
        let other = ChildNetPolicy::new(sandbox, budget, ChildNetwork::Inherit)
            .unwrap()
            .unwrap();
        assert_ne!(other.proxy_userinfo("gh"), userinfo);
        assert!(net.start_proxy(false).unwrap().userinfo().is_none());

        let debug = format!("{proxy:?} {net:?}");
        assert!(!debug.contains(&password), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
    }

    /// A credential goes only to its hosts and paths, over TLS (unless it
    /// allows plaintext): other requests through the same proxy don't get
    /// it, and a plain-HTTP request it applies to is refused rather than
    /// sent without it.
    #[tokio::test]
    async fn credentials_stay_on_their_hosts_paths_and_transport() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = credential_proxy(
            config(&cert),
            &[Credential::bearer(TOKEN)
                .for_hosts(["api.test"])
                .for_paths(["/org/"])],
            false,
        );
        for (host, path, injected) in [
            ("api.test", "/org/repo", true),
            ("api.test", "/other", false),
            ("api.test", "/org%2frepo", false),
            ("other.test", "/org/repo", false),
        ] {
            seen.lock().unwrap().clear();
            let (stream, status) = connect_authorized(&proxy, &format!("{host}:{port}")).await;
            assert!(status.contains(" 200 "), "{status}");
            let mut tls = tls(&proxy, stream, host).await;
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            let _ = exchange(&mut tls, request.as_bytes()).await;
            assert_eq!(received(&seen).contains(TOKEN), injected, "{host}{path}");
        }

        // Plain HTTP doesn't get it: a request it applies to is refused,
        // others go.
        let (port, _, seen) = upstream(false).await;
        use base64::Engine as _;
        let auth = base64::prelude::BASE64_STANDARD.encode(proxy.userinfo().unwrap());
        for (path, status) in [("/org/repo", " 403 "), ("/other", " 200 OK")] {
            let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
            let request = format!(
                "GET http://api.test:{port}{path} HTTP/1.1\r\nHost: api.test:{port}\r\n\
                 Proxy-Authorization: Basic {auth}\r\nConnection: close\r\n\r\n"
            );
            let response = exchange(&mut stream, request.as_bytes()).await;
            assert!(response.contains(status), "{path}: {response}");
        }
        assert!(!received(&seen).contains(TOKEN));
        assert!(!received(&seen).contains("Proxy-Authorization"));
    }

    /// A proxy with credentials only serves clients with its password, and
    /// never opens a blind tunnel to a credential's host.
    #[tokio::test]
    async fn credential_proxies_want_their_password_and_dont_tunnel() {
        let (port, cert, _) = upstream(true).await;
        let credential = Credential::bearer(TOKEN).for_hosts(["api.test"]);
        let proxy = credential_proxy(config(&cert), std::slice::from_ref(&credential), false);
        let (_, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 407 "), "{status}");
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        let response = exchange(
            &mut stream,
            format!(
                "CONNECT api.test:{port} HTTP/1.1\r\nProxy-Authorization: Basic ZXJ5eDp3cm9uZw==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 407 "), "{response}");

        let tunnels = credential_proxy(config(&cert), &[credential], true);
        let (_, status) = connect_authorized(&tunnels, &format!("api.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");
        let (_, status) = connect_authorized(&tunnels, &format!("other.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
    }

    /// No policy reaches a child proxy, even one allowing loopback: not the
    /// guest's own connections, nor a child's through another proxy.
    #[tokio::test]
    async fn child_proxies_are_no_ones_destination() {
        let (_, cert, _) = upstream(true).await;
        let proxy = credential_proxy(
            config(&cert),
            &[Credential::bearer(TOKEN).for_hosts(["api.test"])],
            false,
        );
        let port = proxy.addr().port();
        for addr in ["127.0.0.1", "0.0.0.0", "[::ffff:127.0.0.1]"] {
            let addr: SocketAddr = format!("{addr}:{port}").parse().unwrap();
            assert!(is_child_proxy(addr), "{addr}");
        }
        // IPv6 loopback doesn't reach a proxy listening on 127.0.0.1.
        assert!(!is_child_proxy(format!("[::1]:{port}").parse().unwrap()));

        let open = NetConfig::default()
            .allow_all_hosts(true)
            .with_blocked_hosts(Vec::<String>::new())
            .allow_internal_host("*");
        let policy = NetPolicy::new(open.clone(), HashMap::new()).unwrap();
        let err = policy
            .connect("127.0.0.1", port, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, TcpError::NotPermitted(_)), "{err:?}");

        let other = proxy_with_open(open, &cert);
        let (_, status) = connect_to(&other, &format!("127.0.0.1:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");

        drop(proxy);
        assert!(!is_child_proxy(SocketAddr::from(([127, 0, 0, 1], port))));
    }

    fn proxy_with_open(config: NetConfig, cert: &[u8]) -> ChildProxy {
        proxy(
            config.with_root_cert(cert.to_vec()),
            ChildNetwork::Inherit,
            true,
        )
    }

    /// A narrowed child network allows only what both policies allow.
    #[tokio::test]
    async fn narrow_never_widens() {
        let (port, cert, _) = upstream(true).await;
        let narrow = NetConfig::default()
            .allow_host("other.test")
            .allow_host("evil.test")
            .allow_internal_host("*.test");
        let proxy = proxy(config(&cert), ChildNetwork::Narrow(Box::new(narrow)), false);
        for (host, ok) in [
            ("other.test", true),
            ("api.test", false),
            ("evil.test", false),
        ] {
            let (_, status) = connect_to(&proxy, &format!("{host}:{port}")).await;
            assert_eq!(status.contains(" 200 "), ok, "{host}: {status}");
        }
        assert!(
            ChildProxy::start(config(&cert), secrets(), ChildNetwork::None, false)
                .unwrap()
                .is_none()
        );
    }

    /// Only the CA's certificate is handed out, never its key.
    #[tokio::test]
    async fn only_the_ca_certificate_reaches_the_child() {
        let proxy = proxy(config(&[]), ChildNetwork::Inherit, false);
        let pem = proxy.ca_pem();
        assert_eq!(
            pem.matches("-----BEGIN CERTIFICATE-----").count(),
            1,
            "{pem}"
        );
        assert!(!pem.contains("PRIVATE KEY"), "{pem}");
    }

    /// A refusal's reason never repeats the request's headers, and a
    /// placeholder in what it does repeat is redacted.
    #[tokio::test]
    async fn refusals_never_echo_headers_or_secrets() {
        let proxy = proxy(config(&[]), ChildNetwork::Inherit, false);
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        let request = format!(
            "CONNECT {PH}:443 HTTP/1.1\r\nHost: {PH}:443\r\nProxy-Authorization: Basic c2VjcmV0\r\n\r\n"
        );
        let response = exchange(&mut stream, request.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(response.contains("[REDACTED]"), "{response}");
        for leaked in [PH, REAL, "c2VjcmV0", "Proxy-Authorization"] {
            assert!(!response.contains(leaked), "{leaked}: {response}");
        }
    }

    /// A narrowed network can't add a host, a network or a secret's host
    /// that the sandbox lacks.
    #[tokio::test]
    async fn narrow_gains_nothing() {
        let (port, cert, seen) = upstream(true).await;
        // A narrow config allowing the blocked loopback network: still
        // refused, as the sandbox's own policy blocks it.
        let sandbox = config(&cert).with_internal_hosts(Vec::<String>::new());
        let narrow = NetConfig::permissive().allow_internal_host("*.test");
        let proxy = proxy(sandbox, ChildNetwork::Narrow(Box::new(narrow)), false);
        let (_, status) = connect_to(&proxy, &format!("other.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");

        // The secret is for api.test only: sent to other.test, which both
        // allow, it's refused rather than substituted.
        let narrow = NetConfig::permissive().allow_internal_host("*.test");
        let proxy =
            super::tests::proxy(config(&cert), ChildNetwork::Narrow(Box::new(narrow)), false);
        let (stream, status) = connect_to(&proxy, &format!("other.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut tls = tls(&proxy, stream, "other.test").await;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: other.test\r\nAuthorization: {PH}\r\nConnection: close\r\n\r\n"
        );
        let response = exchange(&mut tls, request.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(!received(&seen).contains(REAL));
    }

    /// A secret scoped by HTTP rules is substituted in a child's request
    /// they match, and a request they don't is refused, through the proxy
    /// as on the sandbox's own connections.
    #[tokio::test]
    async fn secret_http_rules_apply_to_children() {
        let (port, cert, seen) = upstream(true).await;
        let secret = SecretConfig {
            placeholder: PH.to_string(),
            ..SecretConfig::new(REAL, vec!["api.test".into()])
        }
        .with_http_rules([crate::HttpRule::new("api.test").with_paths(["/ok"])]);
        let proxy = ChildProxy::start(
            config(&cert),
            HashMap::from([("KEY".to_string(), secret)]),
            ChildNetwork::Inherit,
            false,
        )
        .unwrap()
        .unwrap();
        let send = async |path: &str| {
            let (stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
            assert!(status.contains(" 200 "), "{status}");
            let mut tls = tls(&proxy, stream, "api.test").await;
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: api.test\r\nAuthorization: {PH}\r\nConnection: close\r\n\r\n"
            );
            exchange(&mut tls, request.as_bytes()).await
        };

        let response = send("/elsewhere").await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(!received(&seen).contains(REAL));

        let response = send("/ok/x").await;
        assert!(response.contains(" 200 OK"), "{response}");
        assert!(received(&seen).contains(&format!("Authorization: {REAL}")));
    }

    /// A permissive narrow config still can't reach a host the sandbox
    /// doesn't allow.
    #[tokio::test]
    async fn permissive_narrow_keeps_the_sandbox_hosts() {
        let (port, cert, _) = upstream(true).await;
        let narrow = NetConfig::permissive().allow_internal_host("*.test");
        let proxy = proxy(config(&cert), ChildNetwork::Narrow(Box::new(narrow)), false);
        let (_, status) = connect_to(&proxy, &format!("evil.test:{port}")).await;
        assert!(status.contains(" 403 "), "{status}");
    }

    /// Points every name at an address nothing listens on.
    #[derive(Debug)]
    struct Elsewhere;

    #[async_trait::async_trait]
    impl Resolver for Elsewhere {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::from(([10, 255, 255, 1], port))])
        }
    }

    /// A narrow config's limits only lower the sandbox's, and its resolver
    /// is ignored.
    #[tokio::test]
    async fn narrow_takes_the_tighter_limits_and_the_sandbox_resolver() {
        let sandbox = Arc::new(
            NetPolicy::new(
                config(&[])
                    .with_max_connections(1)
                    .with_io_timeout(Duration::from_secs(2)),
                HashMap::new(),
            )
            .unwrap(),
        );
        let narrow = sandbox
            .narrowed(NetConfig::permissive().with_connect_timeout(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(narrow.max_connections(), 1);
        assert_eq!(narrow.io_timeout(), Duration::from_secs(2));
        assert_eq!(narrow.connect_timeout(), Duration::from_secs(1));

        let (port, cert, _) = upstream(true).await;
        let narrow = NetConfig::permissive()
            .allow_internal_host("*.test")
            .with_resolver(Elsewhere);
        let proxy = proxy(
            config(&cert).with_max_connections(1),
            ChildNetwork::Narrow(Box::new(narrow)),
            false,
        );
        // Reaches the upstream through the sandbox's resolver.
        let (held, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        // The sandbox's one-connection limit applies while that's open.
        let (_, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 503 "), "{status}");
        drop(held);
    }

    /// The guest and its children share the execution's `max_connections`:
    /// what the guest holds, children can't, through either proxy.
    #[tokio::test]
    async fn guest_and_children_share_one_connection_budget() {
        let (port, cert, _) = upstream(true).await;
        let target = format!("api.test:{port}");
        let mut guest =
            crate::net::ConnectionManager::new(config(&cert).with_max_connections(2), secrets())
                .unwrap();
        let children = guest.child_network(ChildNetwork::Inherit).unwrap().unwrap();
        let inspecting = children.start_proxy(false).unwrap();
        let tunnelling = children.start_proxy(true).unwrap();

        let held = guest.tcp_connect("api.test", port, 0).await.unwrap();
        let (_child, status) = connect_to(&tunnelling, &target).await;
        assert!(status.contains(" 200 "), "{status}");
        // Two of two: neither the other proxy nor the guest gets a third.
        let (_, status) = connect_to(&inspecting, &target).await;
        assert!(status.contains(" 503 "), "{status}");
        let err = guest.tcp_connect("api.test", port, 0).await.unwrap_err();
        assert!(err.to_string().contains("connection limit"), "{err}");

        // The guest closing one hands it back.
        guest.tcp_close(held);
        let (_child, status) = connect_to(&inspecting, &target).await;
        assert!(status.contains(" 200 "), "{status}");
    }

    /// A narrowed limit holds across both of the children's proxies, below
    /// the guest's.
    #[tokio::test]
    async fn narrow_limits_all_the_childrens_proxies_together() {
        let (port, cert, _) = upstream(true).await;
        let target = format!("api.test:{port}");
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets()).unwrap();
        let narrow = NetConfig::permissive()
            .allow_internal_host("*.test")
            .with_max_connections(1);
        let children = guest
            .child_network(ChildNetwork::Narrow(Box::new(narrow)))
            .unwrap()
            .unwrap();
        let inspecting = children.start_proxy(false).unwrap();
        let tunnelling = children.start_proxy(true).unwrap();

        let (_child, status) = connect_to(&inspecting, &target).await;
        assert!(status.contains(" 200 "), "{status}");
        let (_, status) = connect_to(&tunnelling, &target).await;
        assert!(status.contains(" 503 "), "{status}");
    }

    /// The guest and its children share the execution's `max_bytes`, and
    /// the guest's `traffic()` counts both: a child gets a 403 once what's
    /// left won't take its request, and is cut off mid-response at the limit.
    #[tokio::test]
    async fn guest_and_children_share_one_byte_budget() {
        let (port, cert, _) = upstream(false).await;
        let request = format!(
            "GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test:{port}\r\nConnection: close\r\n\r\n"
        );
        let get = |proxy: &ChildProxy| {
            let addr = proxy.addr();
            let request = request.clone();
            async move {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                exchange(&mut stream, request.as_bytes()).await
            }
        };

        // Unlimited: the child's bytes show up in the guest's traffic.
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets()).unwrap();
        let proxy = guest
            .child_network(ChildNetwork::Inherit)
            .unwrap()
            .unwrap()
            .start_proxy(false)
            .unwrap();
        let response = get(&proxy).await;
        assert!(response.contains(" 200 OK"), "{response}");
        let traffic = guest.traffic();
        assert_eq!(traffic.sent, request.len() as u64);
        assert_eq!(traffic.received, response.len() as u64);

        // Room for the request but not the whole echoed response.
        let max = request.len() as u64 + 40;
        let guest =
            crate::net::ConnectionManager::new(config(&cert).with_max_bytes(max), secrets())
                .unwrap();
        let proxy = guest
            .child_network(ChildNetwork::Inherit)
            .unwrap()
            .unwrap()
            .start_proxy(false)
            .unwrap();
        let response = get(&proxy).await;
        assert_eq!(response.len(), 40, "{response}");
        assert_eq!(guest.traffic().sent + guest.traffic().received, max);
        // Spent: the next request is refused before it's sent.
        let response = get(&proxy).await;
        assert!(
            response.starts_with("HTTP/1.1 403 ") && response.contains("network limit reached"),
            "{response}"
        );
        assert_eq!(guest.traffic().sent + guest.traffic().received, max);

        // What the guest sends leaves that much less for its children.
        let mut guest = crate::net::ConnectionManager::new(
            config(&cert).with_max_bytes(request.len() as u64 + 10),
            secrets(),
        )
        .unwrap();
        let proxy = guest
            .child_network(ChildNetwork::Inherit)
            .unwrap()
            .unwrap()
            .start_proxy(false)
            .unwrap();
        let handle = guest.tcp_connect("api.test", port, 0).await.unwrap();
        guest.tcp_write(handle, &[b'x'; 20], 0).await.unwrap();
        let response = get(&proxy).await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    }

    /// Once the byte limit is spent, a request gets its 403 before anything
    /// is dialled, a tunnelled or inspected `CONNECT` included.
    #[tokio::test]
    async fn a_spent_byte_limit_refuses_before_dialling() {
        let (port, cert, _) = upstream(false).await;
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let dialled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&dialled);
        tokio::spawn(async move {
            while target.accept().await.is_ok() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        for tunnels in [false, true] {
            let mut guest =
                crate::net::ConnectionManager::new(config(&cert).with_max_bytes(10), secrets())
                    .unwrap();
            let proxy = guest
                .child_network(ChildNetwork::Inherit)
                .unwrap()
                .unwrap()
                .start_proxy(tunnels)
                .unwrap();
            let handle = guest.tcp_connect("api.test", port, 0).await.unwrap();
            guest.tcp_write(handle, &[b'x'; 10], 0).await.unwrap();

            let (_, status) = connect_to(&proxy, &format!("api.test:{target_port}")).await;
            assert!(status.contains(" 403 "), "tunnels={tunnels}: {status}");
            let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
            let response = exchange(
                &mut stream,
                format!("GET http://api.test:{target_port}/ HTTP/1.1\r\nHost: api.test\r\n\r\n")
                    .as_bytes(),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(dialled.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A request the proxy refuses after charging it is refunded: it never
    /// reached the upstream.
    #[tokio::test]
    async fn refused_requests_are_refunded() {
        let (port, cert, seen) = upstream(true).await;
        let guest =
            crate::net::ConnectionManager::new(config(&cert).with_max_bytes(1 << 20), secrets())
                .unwrap();
        let proxy = guest
            .child_network(ChildNetwork::Inherit)
            .unwrap()
            .unwrap()
            .start_proxy(false)
            .unwrap();
        let (stream, _) = connect_to(&proxy, &format!("api.test:{port}")).await;
        let mut tls = tls(&proxy, stream, "api.test").await;
        let response = exchange(
            &mut tls,
            b"GET / HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert_eq!(received(&seen), "");
        assert_eq!(guest.traffic().sent, 0);
    }

    /// A narrowed `max_bytes` limits the children, below the guest's.
    #[tokio::test]
    async fn narrow_limits_the_childrens_bytes() {
        let (port, cert, _) = upstream(false).await;
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets()).unwrap();
        let narrow = NetConfig::permissive()
            .allow_internal_host("*.test")
            .with_max_bytes(20);
        let proxy = guest
            .child_network(ChildNetwork::Narrow(Box::new(narrow)))
            .unwrap()
            .unwrap()
            .start_proxy(false)
            .unwrap();
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        let request = format!(
            "GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test:{port}\r\nConnection: close\r\n\r\n"
        );
        let response = exchange(&mut stream, request.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 403 ")
                && response.contains("network limit reached: 20 bytes"),
            "{response}"
        );
        assert_eq!(guest.traffic(), crate::net::NetTraffic::default());
    }

    /// A command's narrowed network (`ChildNetPolicy::narrowed`) has its
    /// own limits and still counts against the children's and the
    /// execution's: one connection through it takes the children's only
    /// slot, and its own byte limit refuses what the others would allow.
    #[tokio::test]
    async fn a_narrowed_childrens_network_counts_against_every_budget() {
        let (port, cert, _) = upstream(true).await;
        let target = format!("api.test:{port}");
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets()).unwrap();
        let children = guest
            .child_network(ChildNetwork::Narrow(Box::new(
                NetConfig::permissive()
                    .allow_internal_host("*.test")
                    .with_max_connections(1),
            )))
            .unwrap()
            .unwrap();
        let command = children
            .narrowed(NetConfig::permissive().allow_internal_host("*.test"))
            .unwrap();
        let narrowed = command.start_proxy(false).unwrap();
        let others = children.start_proxy(false).unwrap();
        let (_held, status) = connect_to(&narrowed, &target).await;
        assert!(status.contains(" 200 "), "{status}");
        let (_, status) = connect_to(&others, &target).await;
        assert!(status.contains(" 503 "), "{status}");

        let (port, cert, _) = upstream(false).await;
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets()).unwrap();
        let children = guest.child_network(ChildNetwork::Inherit).unwrap().unwrap();
        let command = children
            .narrowed(
                NetConfig::permissive()
                    .allow_internal_host("*.test")
                    .with_max_bytes(20),
            )
            .unwrap();
        let proxy = command.start_proxy(false).unwrap();
        let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
        let request = format!(
            "GET http://api.test:{port}/ HTTP/1.1\r\nHost: api.test:{port}\r\nConnection: close\r\n\r\n"
        );
        let response = exchange(&mut stream, request.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 403 ")
                && response.contains("network limit reached: 20 bytes"),
            "{response}"
        );
    }

    /// An idle tunnel is closed after the I/O timeout.
    #[tokio::test]
    async fn idle_tunnels_close() {
        let (port, cert, _) = upstream(false).await;
        let proxy = proxy(
            config(&cert).with_io_timeout(Duration::from_millis(200)),
            ChildNetwork::Inherit,
            true,
        );
        let (mut stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
        assert!(matches!(read, Ok(Ok(0))), "{read:?}");
    }

    /// A TLS upstream for `api.test` that reads one request's head and
    /// answers with `script`: each part, then a pause. Returns its port and
    /// certificate (DER).
    async fn scripted_upstream(script: Vec<(Vec<u8>, u64)>) -> (u16, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["api.test".into()]).unwrap();
        let der = cert.cert.der().to_vec();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let acceptor = TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.cert.der().clone()], key)
                .unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while find_header_end(&head).is_none() {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            for (part, pause_ms) in script {
                stream.write_all(&part).await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(pause_ms)).await;
            }
            let _ = stream.shutdown().await;
        });
        (port, der)
    }

    /// GET / with the placeholder through a CONNECT to `api.test`, and the
    /// response the child gets.
    async fn proxied_get(port: u16, cert: &[u8]) -> String {
        let proxy = proxy(config(cert), ChildNetwork::Inherit, false);
        let (stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut tls = tls(&proxy, stream, "api.test").await;
        let request =
            format!("GET / HTTP/1.1\r\nHost: api.test\r\nAuthorization: Bearer {PH}\r\n\r\n");
        exchange(&mut tls, request.as_bytes()).await
    }

    /// The child proxy frames responses like the sandbox's own connections
    /// do: a value in a body is masked whole however long the server pauses
    /// inside it, so how the child gets the bytes says nothing about whether
    /// they start a secret. Before, it held back only what looked like a
    /// secret's start, and released it after 100 ms.
    #[tokio::test]
    async fn proxied_responses_are_masked_by_their_framing() {
        let body = format!("a {REAL} b");
        let split = "a ".len() + 8;
        let (port, cert) = scripted_upstream(vec![
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    &body[..split]
                )
                .into_bytes(),
                300,
            ),
            (body.as_bytes()[split..].to_vec(), 0),
        ])
        .await;
        let response = proxied_get(port, &cert).await;
        assert!(response.ends_with(&format!("a {PH} b")), "{response}");
        assert!(!response.contains(&REAL[..8]), "{response}");

        // Split across chunks, it's masked after de-chunking.
        let (port, cert) = scripted_upstream(vec![
            (
                format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
                     {:x}\r\na {}\r\n",
                    2 + 10,
                    &REAL[..10]
                )
                .into_bytes(),
                30,
            ),
            (
                format!(
                    "{:x}\r\n{} b\r\n0\r\n\r\n",
                    REAL.len() - 10 + 2,
                    &REAL[10..]
                )
                .into_bytes(),
                0,
            ),
        ])
        .await;
        let response = proxied_get(port, &cert).await;
        let (_, mut chunks) = response.split_once("\r\n\r\n").unwrap();
        let mut body = String::new();
        while let Some((size, rest)) = chunks.split_once("\r\n") {
            let size = usize::from_str_radix(size, 16).unwrap();
            body.push_str(&rest[..size]);
            chunks = &rest[size + 2..];
        }
        assert_eq!(body, format!("a {PH} b"), "{response}");
        assert!(!response.contains(&REAL[..10]), "{response}");
    }

    /// Requests through the proxy are framed as strictly as the sandbox's
    /// own: a leading empty line, a malformed chunk, a header name that
    /// isn't a token, and anything after an `Upgrade` are refused (a 403,
    /// or for a body it can't frame a 502), and none of it reaches the
    /// upstream.
    #[tokio::test]
    async fn proxied_requests_are_framed_strictly() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        for request in [
            &b"\r\nGET / HTTP/1.1\r\nHost: api.test\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\nHost: api.test\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"GET / HTTP/1.1\r\nHost: api.test\r\nBad Name: x\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: api.test\r\nUpgrade: x\r\n\r\nnot a request",
        ] {
            let (stream, _) = connect_to(&proxy, &format!("api.test:{port}")).await;
            let mut tls = tls(&proxy, stream, "api.test").await;
            let response = exchange(&mut tls, request).await;
            assert!(
                response.starts_with("HTTP/1.1 403 ") || response.starts_with("HTTP/1.1 502 "),
                "{request:?}: {response}"
            );
        }
        assert_eq!(received(&seen), "");
    }

    /// Request targets outside RFC 9112's forms are refused through the
    /// proxy too, CONNECTed or plain: one with `://` in its query, and an
    /// authority a server could decode into a path.
    #[tokio::test]
    async fn proxied_request_targets_must_be_in_a_request_target_form() {
        let (port, cert, seen) = upstream(true).await;
        let proxy = proxy(config(&cert), ChildNetwork::Inherit, false);
        for request in [
            &b"GET ../secret?x=http://api.test/ HTTP/1.1\r\nHost: api.test\r\n\r\n"[..],
            b"GET http://..%2fsecret/x HTTP/1.1\r\nHost: api.test\r\n\r\n",
        ] {
            let (stream, _) = connect_to(&proxy, &format!("api.test:{port}")).await;
            let mut tls = tls(&proxy, stream, "api.test").await;
            let response = exchange(&mut tls, request).await;
            assert!(
                response.starts_with("HTTP/1.1 403 "),
                "{request:?}: {response}"
            );
        }
        for target in ["http://..%2fsecret/x", "http://%2e%2e%2fsecret/x"] {
            let mut stream = TcpStream::connect(proxy.addr()).await.unwrap();
            let request = format!("GET {target} HTTP/1.1\r\nHost: ..%2fsecret\r\n\r\n");
            let response = exchange(&mut stream, request.as_bytes()).await;
            assert!(
                response.starts_with("HTTP/1.1 400 "),
                "{target}: {response}"
            );
        }
        assert_eq!(received(&seen), "");
    }

    /// Other spellings of a blocked host (case, a trailing dot) are blocked
    /// through the proxy too: its host checks are the sandbox's.
    #[tokio::test]
    async fn proxied_host_spellings_are_checked_alike() {
        let (port, cert, _) = upstream(true).await;
        let proxy = proxy(
            config(&cert).allow_all_hosts(true).block_host("other.test"),
            ChildNetwork::Inherit,
            false,
        );
        for host in ["other.test", "other.test.", "OTHER.Test."] {
            let (_, status) = connect_to(&proxy, &format!("{host}:{port}")).await;
            assert!(status.contains(" 403 "), "{host}: {status}");
        }
        let (_, status) = connect_to(&proxy, &format!("api.test.:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
    }

    /// An upstream that speaks first doesn't turn the connection raw for the
    /// child: what it writes next is still framed and checked (here, it
    /// names another host and isn't allowed by the rules), and none of it
    /// reaches the upstream.
    #[tokio::test]
    async fn a_server_speaking_first_doesnt_unframe_the_child() {
        let cert = rcgen::generate_simple_self_signed(vec!["api.test".into()]).unwrap();
        let der = cert.cert.der().to_vec();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let acceptor = TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.cert.der().clone()], key)
                .unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Received::default();
        let upstream_seen = Arc::clone(&seen);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            stream.write_all(b"hello\r\n").await.unwrap();
            stream.flush().await.unwrap();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => upstream_seen.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        let config =
            config(&der).allow_http(crate::net::HttpRule::new("api.test").with_paths(["/public"]));
        let proxy = proxy(config, ChildNetwork::Inherit, false);
        let (stream, status) = connect_to(&proxy, &format!("api.test:{port}")).await;
        assert!(status.contains(" 200 "), "{status}");
        let mut tls = tls(&proxy, stream, "api.test").await;
        // The greeting reaches the child, so the read side has run first.
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        let _ = tls
            .write_all(b"GET /secret HTTP/1.1\r\nHost: other.test\r\n\r\nnot http at all\r\n")
            .await;
        let _ = tls.flush().await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut rest)).await;
        assert_eq!(received(&seen), "");
    }
}
