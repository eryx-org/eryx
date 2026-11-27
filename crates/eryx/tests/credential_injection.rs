//! Credentials for the script's own requests ([`eryx::Credential::for_script`]):
//! headers the host adds to Python's requests to some hosts, which the
//! script never sees, against a local HTTPS server.
//!
//! ```sh
//! cargo nextest run -p eryx --features embedded --test credential_injection
//! ```
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use eryx::{Credential, ExecuteOptions, IpNet, NetConfig, Resolver, Sandbox};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const ACCESS: &str = "access-token-0123456789abcdef";
const ID: &str = "id-token-fedcba9876543210";

/// Resolves every name to 127.0.0.1, where the test servers listen.
#[derive(Debug)]
struct Localhost;

#[async_trait::async_trait]
impl Resolver for Localhost {
    async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    }
}

/// Every request head a server received.
type Seen = Arc<Mutex<Vec<String>>>;

/// Answer one request by path: `/redirect-other` and `/redirect-same`
/// redirect (to `other.test` on `port`, or `/api/echo`), anything else
/// echoes the request head as the body.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, port: u16, seen: Seen) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    seen.lock().unwrap().push(head.clone());
    let path = head.split(' ').nth(1).unwrap_or("/");
    let response = match path {
        "/api/redirect-other" => format!(
            "HTTP/1.1 302 Found\r\nLocation: https://other.test:{port}/api/echo\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        ),
        "/api/redirect-same" => "HTTP/1.1 302 Found\r\nLocation: /api/echo\r\n\
                                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
        // Close-delimited, since masking changes the echo's length.
        _ => format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{head}"),
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A server on 127.0.0.1, over TLS for `api.test` and `other.test` (its
/// certificate's DER, returned) or plain HTTP.
async fn server(tls: bool) -> (u16, Vec<u8>, Seen) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["api.test".into(), "other.test".into()]).unwrap();
    let der = cert.cert.der().to_vec();
    let key = PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(der.clone())], key)
        .unwrap();
    let acceptor = tls.then(|| TlsAcceptor::from(Arc::new(config)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        if let Ok(stream) = acceptor.accept(stream).await {
                            answer(stream, port, recorded).await;
                        }
                    }
                    None => answer(stream, port, recorded).await,
                }
            });
        }
    });
    (port, der, seen)
}

/// Run `code`, returning its stdout and stderr whether or not it failed.
async fn run(sandbox: &Sandbox, code: &str, options: &ExecuteOptions) -> String {
    let (stdout, stderr) = match sandbox.execute_with(code, options).await {
        Ok(out) => (out.stdout, out.stderr),
        Err(e) => match e.kind() {
            eryx::ErrorKind::ScriptFailed { stdout, stderr, .. } => {
                (stdout.clone(), stderr.clone())
            }
            _ => panic!("{e}"),
        },
    };
    format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    )
}

/// A Python `get(url)` that sends the guest's own copies of the credential's
/// headers, and returns the body or the error.
const GET: &str = r#"
import os, urllib.request
def get(url):
    req = urllib.request.Request(url, headers={"Authorization": "Bearer guest", "X-Id-Token": "guest"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.read().decode()
    except Exception as e:
        return f"error: {e}"
"#;

/// The headers go together to the requests they're for, and only those:
/// to their host and port, under their paths, over TLS. A guest-sent copy is
/// replaced, never kept or doubled; the echoed values come back masked, and
/// the script has no env var, placeholder or error message holding them. A
/// redirect to another host gets nothing; one to the same host is matched
/// again. Over plain HTTP the request is refused rather than sent without
/// them.
#[tokio::test]
async fn script_requests_get_the_headers_their_scope_allows() {
    let (port, cert, seen) = server(true).await;
    let (other_port, cert2, seen2) = server(true).await;
    let (plain_port, _, plain_seen) = server(false).await;
    let sandbox = Sandbox::embedded()
        .with_network(
            NetConfig::default()
                .allow_host("api.test")
                .allow_host("other.test")
                .allow_network("127.0.0.0/8".parse::<IpNet>().unwrap())
                .with_resolver(Localhost)
                .with_root_cert(cert)
                .with_root_cert(cert2),
        )
        .build()
        .unwrap();
    let credential = Credential::headers([
        ("Authorization", format!("Bearer {ACCESS}")),
        ("X-Id-Token", ID.to_string()),
    ])
    .for_hosts([format!("api.test:{port}"), format!("api.test:{plain_port}")])
    .for_paths(["/api/"])
    .for_script();
    let options = ExecuteOptions::new().with_credential(credential);
    let code = format!(
        "{GET}
print('echo:', get('https://api.test:{port}/api/echo'))
print('public:', get('https://api.test:{port}/public'))
print('other host:', get('https://other.test:{port}/api/echo'))
print('other port:', get('https://api.test:{other_port}/api/echo'))
print('redirect other:', get('https://api.test:{port}/api/redirect-other'))
print('redirect same:', get('https://api.test:{port}/api/redirect-same'))
print('plain:', get('http://api.test:{plain_port}/api/echo'))
print('env:', 'REDACTED' in repr(dict(os.environ)))
print('leak: {ACCESS} {ID}')
"
    );
    let out = run(&sandbox, &code, &options).await;

    let requests = seen.lock().unwrap().clone();
    let by_path = |prefix: &str| -> Vec<&String> {
        requests
            .iter()
            .filter(|r| r.starts_with(prefix) && r.contains(&format!("api.test:{port}")))
            .collect()
    };
    // Both headers, once each, the guest's copies gone.
    for head in by_path("GET /api/echo ") {
        assert_eq!(head.matches(ACCESS).count(), 1, "{head}");
        assert_eq!(head.matches(ID).count(), 1, "{head}");
        assert!(!head.contains("guest"), "{head}");
        assert_eq!(
            head.to_ascii_lowercase().matches("authorization:").count(),
            1
        );
        assert_eq!(head.to_ascii_lowercase().matches("x-id-token:").count(), 1);
    }
    // The direct request and the same-host redirect's.
    assert_eq!(by_path("GET /api/echo ").len(), 2, "{requests:?}");
    // Outside their paths, the guest's own headers go as sent.
    let public = by_path("GET /public ");
    assert!(
        public.len() == 1 && public[0].contains("Bearer guest") && !public[0].contains(ACCESS),
        "{public:?}"
    );
    // Not to another host (directly or redirected) or port.
    for head in requests.iter().filter(|r| r.contains("other.test")) {
        assert!(
            !head.contains(ACCESS) && !head.contains(ID),
            "other host got them: {head}"
        );
    }
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.contains("Host: other.test"))
            .count(),
        2,
        "{requests:?}"
    );
    let other_port_seen = seen2.lock().unwrap().clone();
    assert_eq!(other_port_seen.len(), 1, "{other_port_seen:?}");
    assert!(!other_port_seen[0].contains(ACCESS) && other_port_seen[0].contains("guest"));
    // Over plain HTTP, refused, never sent.
    assert!(plain_seen.lock().unwrap().is_empty());
    assert!(
        out.contains("plain: error:") && out.contains("only sent over TLS"),
        "{out}"
    );

    // The echo comes back masked, and nothing in the output holds a value.
    assert!(
        out.contains("Authorization: [REDACTED]") && out.contains("X-Id-Token: [REDACTED]"),
        "{out}"
    );
    assert!(out.contains("env: False"), "{out}");
    assert!(out.contains("leak: [REDACTED] [REDACTED]"), "{out}");
    assert!(!out.contains(ACCESS) && !out.contains(ID), "{out}");
}

/// Without [`Credential::for_script`] the script's requests get nothing:
/// the credential is for children only, and a sandbox with no child
/// network refuses it.
#[tokio::test]
async fn credentials_reach_the_script_only_when_for_it() {
    let (port, cert, seen) = server(true).await;
    let sandbox = Sandbox::embedded()
        .with_network(
            NetConfig::default()
                .allow_host("api.test")
                .allow_network("127.0.0.0/8".parse::<IpNet>().unwrap())
                .with_resolver(Localhost)
                .with_root_cert(cert),
        )
        .build()
        .unwrap();
    let credential = || Credential::bearer(ACCESS).for_hosts(["api.test"]);
    let options = ExecuteOptions::new().with_credential(credential());
    let err = sandbox.execute_with("pass", &options).await.unwrap_err();
    assert!(err.to_string().contains("need a child network"), "{err}");

    let code = format!("{GET}\nprint(get('https://api.test:{port}/api/echo'))");
    let options = ExecuteOptions::new().with_credential(credential().for_script());
    let out = run(&sandbox, &code, &options).await;
    assert!(out.contains("Authorization: [REDACTED]"), "{out}");
    assert!(seen.lock().unwrap()[0].contains(&format!("Bearer {ACCESS}")));
}
