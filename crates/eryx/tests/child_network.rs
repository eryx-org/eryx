//! Child processes' network (#812): commands reach it only through the
//! sandbox's child proxy, under its network policy and secrets.
//!
//! Most tests use the test command (tests/fixtures/test-cmd, which needs
//! `rustup target add wasm32-wasip2`) over the proxy's plain-HTTP path. The
//! HTTPS test runs curl, built with `mise run build-cli -- curl`, from
//! `$ERYX_TEST_CURL` (or `scratch/curl-component/component.wasm`), and is
//! skipped, saying so, when it isn't there.
//!
//! ```sh
//! mise run precompile-runtime-shell
//! cargo nextest run -p eryx --features embedded,shell --test child_network
//! ```
#![cfg(all(feature = "embedded", feature = "shell"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use eryx::language::Shell;
use eryx::{ChildNetwork, Command, NetConfig, Resolver, Sandbox, SandboxBuilder, state};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

const REAL: &str = "real-child-secret-0123456789";

fn test_cmd() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    let wasm = WASM.get_or_init(|| {
        let manifest =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-cmd/Cargo.toml");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("test-cmd");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "wasm32-wasip2", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .expect("running cargo");
        assert!(status.success(), "building the test command failed");
        std::fs::read(target_dir.join("wasm32-wasip2/release/eryx-test-cmd.wasm")).unwrap()
    });
    Command::from_bytes(wasm).unwrap()
}

/// Resolves every name to 127.0.0.1, where the test upstreams listen.
#[derive(Debug)]
struct Localhost;

#[async_trait::async_trait]
impl Resolver for Localhost {
    async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    }
}

type Received = Arc<Mutex<String>>;

/// Answer one request with everything received, as the body.
async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, received: Received) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    received
        .lock()
        .unwrap()
        .push_str(&String::from_utf8_lossy(&head));
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        head.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&head).await;
    let _ = stream.shutdown().await;
}

/// An echoing upstream on 127.0.0.1, over TLS for `api.test` if `tls` is
/// given (a certificate's DER). Returns its port and what it received.
async fn upstream(tls: Option<rustls::ServerConfig>) -> (u16, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Received::default();
    let seen = Arc::clone(&received);
    let acceptor = tls.map(|config| tokio_rustls::TlsAcceptor::from(Arc::new(config)));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        if let Ok(stream) = acceptor.accept(stream).await {
                            echo(stream, seen).await;
                        }
                    }
                    None => echo(stream, seen).await,
                }
            });
        }
    });
    (port, received)
}

/// A network allowing `api.test` (resolved to the test upstreams), with a
/// secret for it.
fn network() -> NetConfig {
    NetConfig::default()
        .allow_host("api.test")
        .allow_internal_host("api.test")
        .with_resolver(Localhost)
}

fn shell(network: NetConfig) -> SandboxBuilder<Shell, state::Has> {
    Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd())
        .with_network(network)
        .with_secret("TOKEN", REAL, vec!["api.test".to_string()])
        .with_child_network(ChildNetwork::Inherit)
}

/// The script's stdout and stderr, whether or not it failed.
async fn run(sandbox: &Sandbox, script: &str) -> String {
    let (stdout, stderr) = match sandbox.execute(script).await {
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

/// Without `with_child_network`, children can't connect anywhere.
#[tokio::test]
async fn children_are_offline_by_default() {
    let (port, _) = upstream(None).await;
    let sandbox = Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("tc connect 127.0.0.1:{port}; echo HTTPS_PROXY=$HTTPS_PROXY"),
    )
    .await;
    assert!(!out.contains("connected"), "{out}");
    assert!(out.contains("error:"), "{out}");
    let out = run(
        &sandbox,
        "tc env HTTPS_PROXY; tc cat /etc/ssl/certs/ca-certificates.crt",
    )
    .await;
    assert!(!out.contains("http://"), "{out}");
    assert!(!out.contains("BEGIN CERTIFICATE"), "{out}");
}

/// `with_child_network` needs the sandbox's network policy.
#[test]
fn child_network_needs_a_network() {
    let err = Sandbox::embedded()
        .shell()
        .with_child_network(ChildNetwork::Inherit)
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("with_network"), "{err}");
}

/// A child gets the proxy, its CA and the secret's placeholder, and can
/// connect to the proxy only.
#[tokio::test]
async fn children_get_the_proxy_and_nothing_else() {
    let (port, _) = upstream(None).await;
    let sandbox = shell(network()).build().unwrap();
    let out = run(
        &sandbox,
        "tc env HTTPS_PROXY; tc env http_proxy; tc env SSL_CERT_FILE; tc env CURL_CA_BUNDLE; \
         tc cat /etc/ssl/certs/ca-certificates.crt; tc env TOKEN",
    )
    .await;
    assert_eq!(out.matches("http://127.0.0.1:").count(), 2, "{out}");
    assert_eq!(
        out.matches("/etc/ssl/certs/ca-certificates.crt").count(),
        2,
        "{out}"
    );
    assert!(
        out.contains("BEGIN CERTIFICATE") && !out.contains("PRIVATE KEY"),
        "{out}"
    );
    // The placeholder, scrubbed from output like any other.
    assert!(out.contains("[REDACTED]") && !out.contains(REAL), "{out}");

    // Not the upstream directly, nor anything but the proxy.
    let out = run(&sandbox, &format!("tc connect 127.0.0.1:{port}")).await;
    assert!(!out.contains("connected"), "{out}");
    // The CA can't be replaced.
    let out = run(&sandbox, "tc write /etc/ssl/certs/ca-certificates.crt x").await;
    assert!(out.contains("error:"), "{out}");
}

/// A request through the proxy reaches the upstream with the real secret
/// (plaintext substitution is on here), and the response the child gets
/// back never has it.
#[tokio::test]
async fn secrets_are_substituted_for_their_hosts_only() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network().allow_plaintext_secrets(true))
        .build()
        .unwrap();
    let out = run(&sandbox, &format!("tc proxy-get http://api.test:{port}/x")).await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(seen.lock().unwrap().contains(&format!("Bearer {REAL}")));
    assert!(!out.contains(REAL), "{out}");

    // Another allowed host isn't one of the secret's.
    let config = network()
        .allow_host("other.test")
        .allow_internal_host("other.test")
        .allow_plaintext_secrets(true);
    let sandbox = shell(config).build().unwrap();
    let before = seen.lock().unwrap().len();
    let out = run(
        &sandbox,
        &format!("tc proxy-get http://other.test:{port}/x"),
    )
    .await;
    assert!(out.contains(" 403 "), "{out}");
    assert_eq!(seen.lock().unwrap().len(), before);
}

/// Hosts the policy refuses get a proxy error, by name or by address.
#[tokio::test]
async fn refused_hosts_get_a_proxy_error() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network()).build().unwrap();
    let out = run(&sandbox, &format!("tc proxy-get http://evil.test:{port}/")).await;
    assert!(out.contains(" 403 "), "{out}");

    // api.test resolves into the blocked loopback network without
    // internal_hosts.
    let config = network().with_internal_hosts(Vec::<String>::new());
    let sandbox = shell(config).build().unwrap();
    let out = run(&sandbox, &format!("tc proxy-get http://api.test:{port}/")).await;
    assert!(
        out.contains(" 403 ") && out.contains("blocked network"),
        "{out}"
    );
    assert_eq!(seen.lock().unwrap().as_str(), "");
}

/// curl from a child reaches an HTTPS host the policy allows, trusting the
/// proxy's CA with no `--cacert`, and the secret reaches only the upstream.
#[tokio::test]
async fn curl_reaches_allowed_hosts_over_https() {
    let curl = std::env::var_os("ERYX_TEST_CURL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scratch/curl-component/component.wasm")
        });
    if !curl.exists() {
        eprintln!(
            "SKIPPED: no curl component at {} (mise run build-cli -- curl, or set ERYX_TEST_CURL)",
            curl.display()
        );
        return;
    }
    let cert = rcgen::generate_simple_self_signed(vec!["api.test".to_string()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.signing_key.serialize_der(),
    ));
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
    let (port, seen) = upstream(Some(tls)).await;
    let sandbox = shell(network().with_root_cert(cert.cert.der().to_vec()))
        .with_command("curl", Command::from_file(&curl).unwrap())
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("curl -sS -w '\\nstatus=%{{http_code}}\\n' -H \"Authorization: Bearer $TOKEN\" https://api.test:{port}/x"),
    )
    .await;
    assert!(
        out.contains("status=200") && out.contains("GET /x"),
        "{out}"
    );
    let got = seen.lock().unwrap().clone();
    assert!(
        got.contains(&format!("Bearer {REAL}")),
        "upstream got {got:?}; child saw {out}"
    );
    assert!(!out.contains(REAL), "{out}");
}
