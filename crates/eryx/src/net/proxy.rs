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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::policy::{NetPolicy, RequestFramer, ResponseMasker};
use super::{
    ConnectionBudget, MAX_HEADER_LEN, NetConfig, TcpError, TlsError, find_header_end, request_line,
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
/// [`ChildNetwork::Narrow`]) and secrets, and its connection budget, which
/// the guest's [`ConnectionManager`](super::ConnectionManager) and every
/// child proxy started from this draw on together. Make one with
/// [`ConnectionManager::child_network`](super::ConnectionManager::child_network).
#[derive(Clone, Debug)]
pub struct ChildNetPolicy {
    policy: Arc<NetPolicy>,
    budget: ConnectionBudget,
    /// The children's own limit, across all their proxies: the narrowed
    /// policy's `max_connections` (for `Inherit`, the same as `budget`'s).
    children: ConnectionBudget,
}

impl ChildNetPolicy {
    /// `None` for [`ChildNetwork::None`].
    pub(crate) fn new(
        sandbox: Arc<NetPolicy>,
        budget: ConnectionBudget,
        network: ChildNetwork,
    ) -> Option<Self> {
        let policy = match network {
            ChildNetwork::None => return None,
            ChildNetwork::Inherit => sandbox,
            ChildNetwork::Narrow(config) => Arc::new(sandbox.narrowed(*config)),
        };
        let children = ConnectionBudget::new(policy.max_connections());
        Some(Self {
            policy,
            budget,
            children,
        })
    }

    /// Start a proxy for children that may (`allow_tunnels`) or may not
    /// open blind tunnels. Must be called within a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Fails if the CA can't be generated or the listener can't be bound.
    pub fn start_proxy(&self, allow_tunnels: bool) -> std::io::Result<ChildProxy> {
        ChildProxy::start_with(self, allow_tunnels)
    }
}

/// A forward proxy for one sandbox's child processes, listening on
/// `127.0.0.1`. It stops when dropped, closing every connection.
#[derive(Debug)]
pub struct ChildProxy {
    addr: SocketAddr,
    ca_pem: String,
    #[cfg(test)]
    ca_der: rustls::pki_types::CertificateDer<'static>,
    task: tokio::task::JoinHandle<()>,
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
    /// Fails if the CA can't be generated or the listener can't be bound.
    pub fn start(
        config: NetConfig,
        secrets: HashMap<String, SecretConfig>,
        network: ChildNetwork,
        allow_tunnels: bool,
    ) -> std::io::Result<Option<Self>> {
        let budget = ConnectionBudget::new(config.max_connections());
        let sandbox = Arc::new(NetPolicy::new(config, secrets));
        ChildNetPolicy::new(sandbox, budget, network)
            .map(|policy| policy.start_proxy(allow_tunnels))
            .transpose()
    }

    fn start_with(net: &ChildNetPolicy, allow_tunnels: bool) -> std::io::Result<Self> {
        let policy = Arc::clone(&net.policy);
        let ca = Ca::new().map_err(std::io::Error::other)?;
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let addr = listener.local_addr()?;
        let ca_pem = ca.pem.clone();
        #[cfg(test)]
        let ca_der = ca.der.clone();
        let shared = Arc::new(Shared {
            connect_timeout: policy.connect_timeout(),
            io_timeout: policy.io_timeout(),
            policy,
            ca,
            allow_tunnels,
            budget: net.budget.clone(),
            children: net.children.clone(),
        });
        let task = tokio::spawn(accept_loop(listener, shared));
        Ok(Self {
            addr,
            ca_pem,
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
}

impl Drop for ChildProxy {
    fn drop(&mut self) {
        // The accept loop owns every connection's task, so they stop too.
        self.task.abort();
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
    /// The execution's connection budget, shared with its guest.
    budget: ConnectionBudget,
    /// Its children's share: `policy`'s limit, across both proxies.
    children: ConnectionBudget,
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
    // Both held for as long as the connection is open.
    let permits = shared
        .children
        .try_acquire()
        .and_then(|child| Ok((child, shared.budget.try_acquire()?)));
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
    let framer = shared.policy.request_framer(host, false).http_only();
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
        if policy.has_http_rules(host) {
            return Err(ProxyError::Refused(format!(
                "{host} has HTTP rules, so it can't be tunnelled"
            )));
        }
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
        splice(client, upstream.into_stream(), shared.io_timeout).await;
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
    let framer = policy.request_framer(host, true).http_only();
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
    let mut masker: ResponseMasker = shared.policy.response_masker();

    if !first.is_empty() {
        let out = framer.frame(&first).map_err(ProxyError::from)?;
        up_w.write_all(&out).await?;
    }

    let mut buf = vec![0u8; BUF_LEN];
    let mut child_done = false;
    loop {
        tokio::select! {
            n = child_r.read(&mut buf), if !child_done => {
                let n = n?;
                if n == 0 {
                    child_done = true;
                    let _ = up_w.shutdown().await;
                    continue;
                }
                match framer.frame(&buf[..n]) {
                    Ok(out) => up_w.write_all(&out).await?,
                    Err(e) => {
                        if let Some(response) = ProxyError::from(e).response(&shared.policy) {
                            let _ = child_w.write_all(&response).await;
                        }
                        let _ = child_w.shutdown().await;
                        return Ok(());
                    }
                }
            }
            data = masker.read(&mut up_r, BUF_LEN as u32, shared.io_timeout) => {
                let data = data?;
                if data.is_empty() {
                    let _ = child_w.shutdown().await;
                    return Ok(());
                }
                child_w.write_all(&data).await?;
            }
        }
    }
}

/// Copy bytes both ways between `a` and `b` until either side closes or
/// neither sends anything for `idle`.
async fn splice<A, B>(a: A, b: B, idle: Duration)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_r, mut a_w) = tokio::io::split(a);
    let (mut b_r, mut b_w) = tokio::io::split(b);
    let mut a_buf = vec![0u8; BUF_LEN];
    let mut b_buf = vec![0u8; BUF_LEN];
    loop {
        let step = tokio::time::timeout(idle, async {
            tokio::select! {
                n = a_r.read(&mut a_buf) => match n {
                    Ok(n) if n > 0 => b_w.write_all(&a_buf[..n]).await.is_ok(),
                    _ => false,
                },
                n = b_r.read(&mut b_buf) => match n {
                    Ok(n) if n > 0 => a_w.write_all(&b_buf[..n]).await.is_ok(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::net::Resolver;
    use rustls::pki_types::ServerName;
    use tokio_rustls::TlsConnector;

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
        let sandbox = Arc::new(NetPolicy::new(
            config(&[])
                .with_max_connections(1)
                .with_io_timeout(Duration::from_secs(2)),
            HashMap::new(),
        ));
        let narrow =
            sandbox.narrowed(NetConfig::permissive().with_connect_timeout(Duration::from_secs(1)));
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
            crate::net::ConnectionManager::new(config(&cert).with_max_connections(2), secrets());
        let children = guest.child_network(ChildNetwork::Inherit).unwrap();
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
        let guest = crate::net::ConnectionManager::new(config(&cert), secrets());
        let narrow = NetConfig::permissive()
            .allow_internal_host("*.test")
            .with_max_connections(1);
        let children = guest
            .child_network(ChildNetwork::Narrow(Box::new(narrow)))
            .unwrap();
        let inspecting = children.start_proxy(false).unwrap();
        let tunnelling = children.start_proxy(true).unwrap();

        let (_child, status) = connect_to(&inspecting, &target).await;
        assert!(status.contains(" 200 "), "{status}");
        let (_, status) = connect_to(&tunnelling, &target).await;
        assert!(status.contains(" 503 "), "{status}");
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
}
