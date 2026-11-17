//! Child processes' network (#812): commands reach it only through the
//! sandbox's child proxy, under its network policy and secrets.
//!
//! The children are small fixtures built by the tests (they need
//! `rustup target add wasm32-wasip2`): tests/fixtures/test-cmd over the
//! proxy's plain-HTTP path, and tests/fixtures/https-get, a rustls client
//! using only the proxy's env vars, over HTTPS. (eryx-org/eryx-commands'
//! curl works the same way; see #812.)
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

/// Build the wasip2 fixture `name` (tests/fixtures/<name>), whose binary is
/// `bin`.
fn fixture(name: &str, bin: &str) -> Vec<u8> {
    fixture_for(name, bin, "wasm32-wasip2")
}

/// Build fixture `name` for `target`.
fn fixture_for(name: &str, bin: &str, target: &str) -> Vec<u8> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .join("Cargo.toml");
    let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--release", "--locked"])
        .args(["--target", target, "--manifest-path"])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building the {name} fixture failed");
    std::fs::read(target_dir.join(format!("{target}/release/{bin}.wasm"))).unwrap()
}

fn test_cmd() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    Command::from_bytes(WASM.get_or_init(|| fixture("test-cmd", "eryx-test-cmd"))).unwrap()
}

/// The test command built for WASI 0.3 (`rustup target add wasm32-wasip3`).
fn test_cmd_p3() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    Command::from_bytes(
        WASM.get_or_init(|| fixture_for("test-cmd", "eryx-test-cmd", "wasm32-wasip3")),
    )
    .unwrap()
}

/// A rustls HTTPS client going through `$HTTPS_PROXY` and trusting
/// `$SSL_CERT_FILE` (tests/fixtures/https-get).
fn https_get() -> Command {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    Command::from_bytes(WASM.get_or_init(|| fixture("https-get", "eryx-test-https-get"))).unwrap()
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

/// A certificate (DER) for `api.test`, and a server config presenting it.
fn tls_config() -> (Vec<u8>, rustls::ServerConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["api.test".to_string()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.signing_key.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
    (cert.cert.der().to_vec(), config)
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
        .with_command("tc3", test_cmd_p3())
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

/// A plain-HTTP request reaches the upstream in origin-form, as a client
/// talking to it directly would send it, not in the absolute form the child
/// sent the proxy.
#[tokio::test]
async fn plain_http_requests_reach_the_upstream_in_origin_form() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network().allow_plaintext_secrets(true))
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("tc proxy-get http://api.test:{port}/x?y=1"),
    )
    .await;
    assert!(out.contains(" 200 OK"), "{out}");
    let seen = seen.lock().unwrap().clone();
    assert!(seen.starts_with("GET /x?y=1 HTTP/1.1\r\n"), "{seen}");
    assert!(
        seen.contains(&format!("Host: api.test:{port}\r\n")),
        "{seen}"
    );
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

/// An HTTPS client in a child reaches a host the policy allows, trusting
/// the proxy's CA from `$SSL_CERT_FILE` alone (the env vars are enough), and
/// the secret reaches only the upstream.
#[tokio::test]
async fn https_clients_reach_allowed_hosts_with_only_the_env() {
    let (cert, tls) = tls_config();
    let (port, seen) = upstream(Some(tls)).await;
    let sandbox = shell(network().with_root_cert(cert))
        .with_command("https-get", https_get())
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("https-get https://api.test:{port}/x \"Authorization: Bearer $TOKEN\""),
    )
    .await;
    assert!(out.contains(" 200 OK") && out.contains("GET /x"), "{out}");
    let got = seen.lock().unwrap().clone();
    assert!(
        got.contains(&format!("Bearer {REAL}")),
        "upstream got {got:?}; child saw {out}"
    );
    assert!(!out.contains(REAL), "{out}");

    // A host the policy refuses gets the proxy's 403 to its CONNECT.
    let out = run(&sandbox, &format!("https-get https://evil.test:{port}/")).await;
    assert!(out.contains(" 403 "), "{out}");
}

/// A child can hold only so many sockets at once, and failing to get more
/// is an ordinary error; they're given back when it exits.
#[tokio::test]
async fn sockets_are_limited() {
    let sandbox = shell(network()).build().unwrap();
    let out = run(&sandbox, "tc hold 100").await;
    assert!(out.starts_with("held 32 error:"), "{out}");
    let out = run(&sandbox, "tc hold 10; tc hold 10").await;
    assert_eq!(out, "held 10\nheld 10\n");
}

/// A script can't point a child's proxy or CA variables elsewhere.
#[tokio::test]
async fn the_proxy_env_cant_be_overridden() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network().allow_plaintext_secrets(true))
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!(
            "HTTPS_PROXY=http://x HTTP_PROXY=http://10.0.0.1:1 SSL_CERT_FILE=/x tc env HTTPS_PROXY; \
             HTTP_PROXY=http://10.0.0.1:1 tc env SSL_CERT_FILE; \
             HTTP_PROXY=http://10.0.0.1:1 tc proxy-get http://api.test:{port}/"
        ),
    )
    .await;
    assert!(out.starts_with("http://127.0.0.1:"), "{out}");
    assert!(
        out.contains("/etc/ssl/certs/ca-certificates.crt\n"),
        "{out}"
    );
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(seen.lock().unwrap().contains(REAL));
}

/// A command allowed tunnels gets a blind one to an allowed host (no
/// substitution), and a command without can't use one: its CONNECTs are
/// inspected, so plain bytes inside are refused, and it can't reach the
/// tunnelling proxy either.
#[tokio::test]
async fn tunnels_are_per_command() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network())
        .with_command("tt", test_cmd().allow_tunnels())
        .build()
        .unwrap();
    let out = run(&sandbox, &format!("tt tunnel-get api.test:{port}")).await;
    assert!(
        out.contains(" 200 Connection established") && out.contains(" 200 OK"),
        "{out}"
    );
    let got = seen.lock().unwrap().clone();
    assert!(
        got.contains("Bearer ERYX_SECRET_PLACEHOLDER_") && !got.contains(REAL),
        "{got}"
    );

    // The inspecting proxy expects TLS after its 200; plain HTTP is refused.
    let (cert, tls) = tls_config();
    let (tls_port, tls_seen) = upstream(Some(tls)).await;
    let inspected = shell(network().with_root_cert(cert)).build().unwrap();
    let out = run(&inspected, &format!("tc tunnel-get api.test:{tls_port}")).await;
    assert!(
        out.contains(" 200 Connection established") && !out.contains(" 200 OK"),
        "{out}"
    );
    assert_eq!(tls_seen.lock().unwrap().as_str(), "");

    let out = run(&sandbox, "p=$(tt env HTTPS_PROXY); tc connect ${p#http://}").await;
    assert!(!out.contains("connected"), "{out}");
}

/// A child can't listen for connections.
#[tokio::test]
async fn children_cant_listen() {
    let sandbox = shell(network()).build().unwrap();
    let out = run(&sandbox, "tc listen").await;
    assert!(
        !out.contains("listening") && out.contains("error:"),
        "{out}"
    );
}

/// A session's children get a proxy in each execution too.
#[tokio::test]
async fn sessions_proxy_each_execution() {
    use eryx::session::{InProcessSession, Session};

    let (port, seen) = upstream(None).await;
    let sandbox = shell(network().allow_plaintext_secrets(true))
        .build()
        .unwrap();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    for _ in 0..2 {
        let out = session
            .execute(&format!("tc proxy-get http://api.test:{port}/"))
            .await
            .unwrap();
        assert!(
            out.stdout_text().contains(" 200 OK"),
            "{}",
            out.stdout_text()
        );
    }
    assert_eq!(seen.lock().unwrap().matches(REAL).count(), 2);
}

/// A pool's shell sandboxes keep the child network, with each lease's
/// network policy and secrets; a lease without one leaves children offline.
#[tokio::test]
async fn pooled_sandboxes_use_each_leases_network() {
    let (port, seen) = upstream(None).await;
    let builder = Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd())
        .with_child_network(ChildNetwork::Inherit);
    let pool = eryx::SandboxPool::new(builder, eryx::PoolConfig::default().with_max_size(1))
        .await
        .unwrap();
    let secrets = std::collections::HashMap::from([(
        "TOKEN".to_string(),
        eryx::SecretConfig::new(REAL, vec!["api.test".to_string()]),
    )]);
    let lease = pool
        .acquire()
        .await
        .unwrap()
        .with_network(network().allow_plaintext_secrets(true), secrets)
        .unwrap();
    let out = lease
        .execute(&format!("tc proxy-get http://api.test:{port}/"))
        .await
        .unwrap();
    assert!(
        out.stdout_text().contains(" 200 OK"),
        "{}",
        out.stdout_text()
    );
    assert!(seen.lock().unwrap().contains(REAL));
    drop(lease);

    let lease = pool.acquire().await.unwrap();
    let out = match lease
        .execute("tc env HTTPS_PROXY; tc connect 127.0.0.1:1")
        .await
    {
        Ok(out) => out.stdout_text(),
        Err(e) => e.to_string(),
    };
    assert!(
        !out.contains("http://") && !out.contains("connected"),
        "{out}"
    );
}

/// A WASI 0.3 child is offline without `with_child_network`, and with it
/// gets the proxy, its CA and nothing else, as a 0.2 one does.
#[tokio::test]
async fn p3_children_get_the_proxy_and_nothing_else() {
    let (port, seen) = upstream(None).await;
    let sandbox = Sandbox::embedded()
        .shell()
        .with_command("tc3", test_cmd_p3())
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("tc3 connect 127.0.0.1:{port}; tc3 env HTTPS_PROXY"),
    )
    .await;
    assert!(
        !out.contains("connected") && out.contains("error:") && !out.contains("http://"),
        "{out}"
    );

    let sandbox = shell(network().allow_plaintext_secrets(true))
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        "tc3 env HTTPS_PROXY; tc3 env SSL_CERT_FILE; tc3 cat /etc/ssl/certs/ca-certificates.crt",
    )
    .await;
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    assert!(out.contains("BEGIN CERTIFICATE"), "{out}");
    let out = run(&sandbox, &format!("tc3 proxy-get http://api.test:{port}/x")).await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(seen.lock().unwrap().contains(&format!("Bearer {REAL}")));
    assert!(!out.contains(REAL), "{out}");
    // Not the upstream directly, and no listening.
    let out = run(&sandbox, &format!("tc3 connect 127.0.0.1:{port}")).await;
    assert!(!out.contains("connected"), "{out}");
    let out = run(&sandbox, "tc3 listen").await;
    assert!(
        !out.contains("listening") && out.contains("error:"),
        "{out}"
    );
}

/// WASI 0.3 sockets count against the same limits as 0.2 ones: a child
/// can't hold a 33rd, and its sockets are given back when it exits.
#[tokio::test]
async fn p3_sockets_are_limited() {
    let sandbox = shell(network()).build().unwrap();
    let out = run(&sandbox, "tc3 hold 100").await;
    assert!(out.starts_with("held 32 error:"), "{out}");
    let out = run(&sandbox, "tc3 hold 10; tc3 hold 10; tc hold 10").await;
    assert_eq!(out, "held 10\nheld 10\nheld 10\n");
}

/// Opt-in: a Go-built `gh` (WASI 0.3) runs as a child and reaches
/// api.github.com through the child proxy. Set ERYX_TEST_GH_WASM to its
/// component: eryx-org/eryx-commands' published `eryx/gh`, as CI pulls it. Needs internet access; a placeholder token gets a real 401.
/// gh needs a writable config dir, which children only get from the VFS.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
#[ignore = "needs ERYX_TEST_GH_WASM and internet access"]
async fn p3_gh_reaches_github_through_the_proxy() {
    let gh = std::env::var_os("ERYX_TEST_GH_WASM").expect("ERYX_TEST_GH_WASM");
    let sandbox = Sandbox::embedded()
        .shell()
        .with_command("gh", Command::from_file(gh).unwrap())
        .with_network(NetConfig::default().allow_host("api.github.com"))
        .with_child_network(ChildNetwork::Inherit)
        .build()
        .unwrap();
    let env = "HOME=/tmp GH_CONFIG_DIR=/tmp/gh GH_NO_UPDATE_NOTIFIER=1 TERM=dumb";
    let out = run(&sandbox, &format!("{env} gh --version")).await;
    assert!(out.contains("gh version"), "{out}");
    let out = run(&sandbox, &format!("{env} GH_TOKEN=placeholder gh api /zen")).await;
    assert!(out.contains("HTTP 401"), "{out}");
}

/// Nested children draw on the execution's budget too (#945, #899): their
/// bytes count in its stats and against `max_bytes`, together with the
/// shell's children's.
#[tokio::test]
async fn nested_children_share_the_executions_budget() {
    let (port, _) = upstream(None).await;
    let script = format!("tc spawn tc proxy-get http://api.test:{port}/x");
    let build = |config: NetConfig| {
        shell(config)
            .with_command("tc", test_cmd().allow_spawn())
            .build()
            .unwrap()
    };

    // One nested exchange, unlimited: what it moved shows up in the stats.
    let result = build(network()).execute(&script).await.unwrap();
    assert!(result.stdout_utf8().unwrap().contains(" 200 OK"));
    let moved = result.stats.network_bytes_sent + result.stats.network_bytes_received;
    assert!(result.stats.network_bytes_sent > 0 && result.stats.network_bytes_received > 0);

    // Exactly that much: the nested exchange spends it, and the shell's own
    // child is refused after.
    let sandbox = build(network().with_max_bytes(moved));
    let out = run(
        &sandbox,
        &format!("{script}; tc proxy-get http://api.test:{port}/x"),
    )
    .await;
    assert_eq!(out.matches(" 200 OK").count(), 1, "{out}");
    assert!(
        out.contains(" 403 ") && out.contains("network limit reached"),
        "{out}"
    );
}

/// A nested child (see `Command::allow_spawn`) gets the same network as the
/// shell's children: the proxy only, with the secret substituted, and the
/// same socket limits.
#[tokio::test]
async fn nested_children_get_the_same_network() {
    let (port, seen) = upstream(None).await;
    let sandbox = shell(network().allow_plaintext_secrets(true))
        .with_command("tc", test_cmd().allow_spawn())
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("tc spawn tc proxy-get http://api.test:{port}/x"),
    )
    .await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(seen.lock().unwrap().contains(&format!("Bearer {REAL}")));
    assert!(!out.contains(REAL), "{out}");
    let out = run(&sandbox, &format!("tc spawn tc connect 127.0.0.1:{port}")).await;
    assert!(!out.contains("connected"), "{out}");
    let out = run(&sandbox, "tc spawn tc hold 100").await;
    assert!(out.starts_with("held 32 error:"), "{out}");
}

/// The script's stdout and stderr under `options`, whether or not it failed.
async fn run_with(sandbox: &Sandbox, script: &str, options: &eryx::ExecuteOptions) -> String {
    let (stdout, stderr) = match sandbox.execute_with(script, options).await {
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

/// One command's network can be taken away or narrowed for an execution,
/// leaving the others' as they are, and the next execution gets it back.
#[tokio::test]
async fn command_options_narrow_one_commands_network() {
    let (port, _) = upstream(None).await;
    let sandbox = shell(network())
        .with_command("ta", test_cmd())
        .build()
        .unwrap();
    let get = |cmd: &str| format!("{cmd} proxy-get http://api.test:{port}/x");

    let offline = eryx::ExecuteOptions::new().with_command_options(
        "ta",
        eryx::CommandOptions::new().with_network(ChildNetwork::None),
    );
    let out = run_with(
        &sandbox,
        &format!("{}; ta env HTTPS_PROXY", get("tc")),
        &offline,
    )
    .await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(!out.contains("http://127.0.0.1:"), "{out}");
    let out = run_with(&sandbox, &get("ta"), &offline).await;
    assert!(!out.contains(" 200 OK") && out.contains("error:"), "{out}");

    // A narrow config allowing no hosts: refused by its own proxy.
    let narrowed = eryx::ExecuteOptions::new().with_command_options(
        "ta",
        eryx::CommandOptions::new().with_network(ChildNetwork::Narrow(Box::default())),
    );
    let out = run_with(
        &sandbox,
        &format!("{}; {}", get("tc"), get("ta")),
        &narrowed,
    )
    .await;
    assert_eq!(out.matches(" 200 OK").count(), 1, "{out}");
    assert!(out.contains(" 403 "), "{out}");

    // Options last one execution.
    let out = run(&sandbox, &get("ta")).await;
    assert!(out.contains(" 200 OK"), "{out}");
}

/// A command registered with tunnels or spawning can lose them for one
/// execution.
#[tokio::test]
async fn command_options_take_away_tunnels_and_spawning() {
    let (port, _) = upstream(None).await;
    let sandbox = shell(network())
        .with_command("tt", test_cmd().allow_tunnels())
        .with_command("ts", test_cmd().allow_spawn())
        .build()
        .unwrap();
    let options = eryx::ExecuteOptions::new()
        .with_command_options("tt", eryx::CommandOptions::new().without_tunnels())
        .with_command_options("ts", eryx::CommandOptions::new().without_spawn());

    // Inspected instead of tunnelled: the proxy wants TLS from the plain
    // HTTP upstream, and answers 502 when there's none.
    let tunnel = format!("tt tunnel-get api.test:{port}");
    let out = run(&sandbox, &tunnel).await;
    assert!(out.contains(" 200 OK"), "{out}");
    let out = run_with(&sandbox, &tunnel, &options).await;
    assert!(out.contains(" 502 ") && !out.contains(" 200 OK"), "{out}");

    let out = run(&sandbox, "ts spawn tc env PWD").await;
    assert!(out.contains('/') && !out.contains("error:"), "{out}");
    let out = run_with(&sandbox, "ts spawn tc env PWD", &options).await;
    assert!(out.contains("error: spawn-failed"), "{out}");
}

/// Options naming a command that isn't registered, or giving a command
/// network the execution doesn't have, fail the execution before it runs.
#[tokio::test]
async fn command_options_can_only_take_away() {
    let sandbox = shell(network()).build().unwrap();
    let unknown = eryx::ExecuteOptions::new()
        .with_command_options("nope", eryx::CommandOptions::new().without_spawn());
    let err = sandbox.execute_with("true", &unknown).await.unwrap_err();
    assert!(
        err.to_string().contains("isn't a registered command"),
        "{err}"
    );

    let offline = Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap();
    let widen = eryx::ExecuteOptions::new().with_command_options(
        "tc",
        eryx::CommandOptions::new().with_network(ChildNetwork::Inherit),
    );
    let err = offline.execute_with("true", &widen).await.unwrap_err();
    assert!(err.to_string().contains("can't have network"), "{err}");
    // Taking away what isn't there is fine.
    let none = eryx::ExecuteOptions::new().with_command_options(
        "tc",
        eryx::CommandOptions::new().with_network(ChildNetwork::None),
    );
    offline.execute_with("true", &none).await.unwrap();
}

/// A narrowed child network is validated like the sandbox's: a pattern that
/// names no host (here a lone `.`) fails the build, or the execution for a
/// command's options, instead of silently matching nothing.
#[tokio::test]
async fn narrowed_child_networks_are_validated() {
    let degenerate = || Box::new(NetConfig::default().block_host("."));
    let err = shell(network())
        .with_child_network(ChildNetwork::Narrow(degenerate()))
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("child network:"), "{err}");

    let sandbox = shell(network()).build().unwrap();
    let options = eryx::ExecuteOptions::new().with_command_options(
        "tc",
        eryx::CommandOptions::new().with_network(ChildNetwork::Narrow(degenerate())),
    );
    let err = sandbox.execute_with("true", &options).await.unwrap_err();
    assert!(err.to_string().contains("command tc's network:"), "{err}");
}

/// A command's network set at registration applies in every execution:
/// taken away, or narrowed within the sandbox's (a narrowed config can't
/// reach a host the sandbox's doesn't allow).
#[tokio::test]
async fn registered_networks_apply_to_every_execution() {
    let (port, _) = upstream(None).await;
    let other = Box::new(
        NetConfig::default()
            .allow_host("other.test")
            .allow_internal_host("other.test"),
    );
    let sandbox = shell(network())
        .with_command("ta", test_cmd().with_network(ChildNetwork::None))
        .with_command("tn", test_cmd().with_network(ChildNetwork::Narrow(other)))
        .build()
        .unwrap();
    let get = |cmd: &str, host: &str| format!("{cmd} proxy-get http://{host}:{port}/x");
    for _ in 0..2 {
        let out = run(&sandbox, &get("tc", "api.test")).await;
        assert!(out.contains(" 200 OK"), "{out}");
        let out = run(
            &sandbox,
            &format!("ta env HTTPS_PROXY; {}", get("ta", "api.test")),
        )
        .await;
        assert!(!out.contains("http://127.0.0.1:"), "{out}");
        assert!(!out.contains(" 200 OK") && out.contains("error:"), "{out}");
        // Neither what only the narrowed config allows, nor what only the
        // sandbox's does.
        let out = run(
            &sandbox,
            &format!("{}; {}", get("tn", "other.test"), get("tn", "api.test")),
        )
        .await;
        assert_eq!(out.matches(" 403 ").count(), 2, "{out}");
    }
}

/// An execution's command options replace the network the command was
/// registered with, for that execution only.
#[tokio::test]
async fn command_options_override_registered_networks() {
    let (port, _) = upstream(None).await;
    let sandbox = shell(network())
        .with_command("ta", test_cmd().with_network(ChildNetwork::None))
        .with_command(
            "tn",
            test_cmd().with_network(ChildNetwork::Narrow(Box::default())),
        )
        .build()
        .unwrap();
    let get = |cmd: &str| format!("{cmd} proxy-get http://api.test:{port}/x");
    let options = eryx::ExecuteOptions::new()
        .with_command_options(
            "ta",
            eryx::CommandOptions::new().with_network(ChildNetwork::Inherit),
        )
        .with_command_options(
            "tn",
            eryx::CommandOptions::new().with_network(ChildNetwork::Inherit),
        );
    let out = run_with(&sandbox, &format!("{}; {}", get("ta"), get("tn")), &options).await;
    assert_eq!(out.matches(" 200 OK").count(), 2, "{out}");

    let out = run(&sandbox, &format!("{}; {}", get("ta"), get("tn"))).await;
    assert!(!out.contains(" 200 OK"), "{out}");
    assert!(out.contains("error:") && out.contains(" 403 "), "{out}");
}

/// A command can't be registered with network the sandbox doesn't give
/// children, nor with an invalid narrowed config: the build fails.
#[test]
fn registered_networks_cant_exceed_the_sandboxs() {
    let err = Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd().with_network(ChildNetwork::Inherit))
        .with_network(network())
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("can't have network"), "{err}");
    let err = Sandbox::embedded()
        .shell()
        .with_command(
            "tc",
            test_cmd().with_network(ChildNetwork::Narrow(Box::default())),
        )
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("can't have network"), "{err}");
    // Taking away what isn't there is fine.
    Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd().with_network(ChildNetwork::None))
        .build()
        .unwrap();

    let degenerate = Box::new(NetConfig::default().block_host("."));
    let err = shell(network())
        .with_command(
            "tn",
            test_cmd().with_network(ChildNetwork::Narrow(degenerate)),
        )
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("command tn's network:"), "{err}");
}

/// Sessions, which take no execute options, use registered networks too.
#[tokio::test]
async fn sessions_use_registered_networks() {
    use eryx::session::{InProcessSession, Session};

    let (port, _) = upstream(None).await;
    let sandbox = shell(network())
        .with_command("ta", test_cmd().with_network(ChildNetwork::None))
        .with_command(
            "tn",
            test_cmd().with_network(ChildNetwork::Narrow(Box::default())),
        )
        .build()
        .unwrap();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    let get = |cmd: &str| format!("{cmd} proxy-get http://api.test:{port}/x");
    for _ in 0..2 {
        let out = session
            .execute(&format!("{}; {}; {}", get("tc"), get("ta"), get("tn")))
            .await
            .map_or_else(|e| e.to_string(), |out| out.stdout_text());
        assert_eq!(out.matches(" 200 OK").count(), 1, "{out}");
        assert!(out.contains(" 403 "), "{out}");
    }
}

/// A `SandboxPool::with_builder` pool's sandboxes can have a child network
/// with `with_network_per_lease`, and use each lease's network; a lease
/// without one leaves children offline, whatever the commands' networks.
#[tokio::test]
async fn builder_pools_use_each_leases_network() {
    let builder = || {
        Sandbox::embedded()
            .shell()
            .with_command("tc", test_cmd())
            .with_command(
                "tn",
                test_cmd().with_network(ChildNetwork::Narrow(Box::new(
                    NetConfig::default()
                        .allow_host("api.test")
                        .allow_internal_host("api.test"),
                ))),
            )
            .with_child_network(ChildNetwork::Inherit)
    };
    let config = || eryx::PoolConfig::default().with_max_size(1);

    // Without it, the build fails as it does outside a pool.
    let err = eryx::SandboxPool::with_builder(move || builder().build(), config())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("with_network"), "{err}");

    let (port, seen) = upstream(None).await;
    let pool = eryx::SandboxPool::with_builder(
        move || builder().with_network_per_lease().build(),
        config(),
    )
    .await
    .unwrap();
    let secrets = std::collections::HashMap::from([(
        "TOKEN".to_string(),
        eryx::SecretConfig::new(REAL, vec!["api.test".to_string()]),
    )]);
    let lease = pool
        .acquire()
        .await
        .unwrap()
        .with_network(network().allow_plaintext_secrets(true), secrets)
        .unwrap();
    let get = |cmd: &str| format!("{cmd} proxy-get http://api.test:{port}/x");
    let out = lease
        .execute(&format!("{}; {}", get("tc"), get("tn")))
        .await
        .unwrap();
    assert_eq!(
        out.stdout_text().matches(" 200 OK").count(),
        2,
        "{}",
        out.stdout_text()
    );
    assert!(seen.lock().unwrap().contains(REAL));
    drop(lease);

    let lease = pool.acquire().await.unwrap();
    let out = lease
        .execute("tc env HTTPS_PROXY; tn env HTTPS_PROXY; tc connect 127.0.0.1:1; tn connect 127.0.0.1:1")
        .await
        .map_or_else(|e| e.to_string(), |out| out.stdout_text());
    assert!(
        !out.contains("http://") && !out.contains("connected"),
        "{out}"
    );
}

/// A credential only the proxy holds (see [`eryx::Credential`]).
const INJECTED: &str = "injected-credential-0123456789";

/// A credential goes to the requests of the commands it's for, and only
/// those: never into any child's env or output, and not to another command
/// or a command it's taken from for one execution. Unscoped, it goes to
/// every child.
#[tokio::test]
async fn credentials_go_to_their_commands_only() {
    let (port, seen) = upstream(None).await;
    let get = |cmd: &str| format!("{cmd} proxy-get http://api.test:{port}/x");
    let credential = || {
        eryx::Credential::bearer(INJECTED)
            .for_hosts(["api.test"])
            .allow_plaintext()
    };
    let sandbox = shell(network())
        .with_credential(credential().for_commands(["tc"]))
        .build()
        .unwrap();
    let out = run(&sandbox, &get("tc")).await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(
        seen.lock()
            .unwrap()
            .contains(&format!("Authorization: Bearer {INJECTED}")),
        "{}",
        seen.lock().unwrap()
    );
    // Masked out of the echo; the child sent its own (placeholder) bearer,
    // which the credential replaced.
    assert!(
        !out.contains(INJECTED) && out.contains("[REDACTED]"),
        "{out}"
    );

    seen.lock().unwrap().clear();
    let out = run(
        &sandbox,
        &format!("{}; tc env HTTP_PROXY; tc3 env HTTP_PROXY", get("tc3")),
    )
    .await;
    assert!(out.contains(" 200 OK"), "{out}");
    assert!(!seen.lock().unwrap().contains(INJECTED));
    // tc's proxy wants a password, tc3's (another proxy) doesn't.
    assert!(
        out.contains("http://eryx:") && out.contains("\nhttp://127.0.0.1:"),
        "{out}"
    );
    assert!(!out.contains(INJECTED), "{out}");

    // Taken away for one execution, and given for one.
    seen.lock().unwrap().clear();
    let without = eryx::ExecuteOptions::new()
        .with_command_options("tc", eryx::CommandOptions::new().without_credentials());
    let out = run_with(&sandbox, &get("tc"), &without).await;
    assert!(out.contains(" 200 OK") && !seen.lock().unwrap().contains(INJECTED));
    let plain = shell(network()).build().unwrap();
    let given = eryx::ExecuteOptions::new().with_credential(credential());
    let out = run_with(&plain, &format!("{}; {}", get("tc"), get("tc3")), &given).await;
    assert_eq!(out.matches(" 200 OK").count(), 2, "{out}");
    assert_eq!(seen.lock().unwrap().matches(INJECTED).count(), 2);
}

/// Over TLS (the proxy's own), a credential replaces the child's header and
/// is masked out of the response: the child never sees it.
#[tokio::test]
async fn credentials_are_injected_over_inspected_tls() {
    let (cert, tls) = tls_config();
    let (port, seen) = upstream(Some(tls)).await;
    let sandbox = shell(network().with_root_cert(cert))
        .with_command("https-get", https_get())
        .with_credential(
            eryx::Credential::token(INJECTED)
                .for_hosts(["api.test"])
                .for_commands(["https-get"]),
        )
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        &format!("https-get https://api.test:{port}/x \"Authorization: token dummy\""),
    )
    .await;
    assert!(out.contains(" 200 OK"), "{out}");
    let got = seen.lock().unwrap().clone();
    assert!(got.contains(&format!("Authorization: token {INJECTED}")) && !got.contains("dummy"));
    assert!(
        !out.contains(INJECTED) && out.contains("Authorization: [REDACTED]"),
        "{out}"
    );
}

/// A credential that can't be used fails the execution before it runs: for
/// an unregistered command, a command with tunnels or no network, or with
/// no child network at all.
#[tokio::test]
async fn unusable_credentials_fail_the_execution() {
    let credential = || eryx::Credential::bearer(INJECTED).for_hosts(["api.test"]);
    let sandbox = shell(network())
        .with_command("tt", test_cmd().allow_tunnels())
        .build()
        .unwrap();
    for (options, why) in [
        (
            eryx::ExecuteOptions::new().with_credential(credential().for_commands(["nope"])),
            "isn't a registered command",
        ),
        (
            eryx::ExecuteOptions::new().with_credential(credential().for_commands(["tt"])),
            "blind tunnels",
        ),
        (
            eryx::ExecuteOptions::new()
                .with_credential(credential().for_commands(["tc"]))
                .with_command_options(
                    "tc",
                    eryx::CommandOptions::new().with_network(ChildNetwork::None),
                ),
            "no network",
        ),
        (
            eryx::ExecuteOptions::new().with_credential(eryx::Credential::bearer(INJECTED)),
            "no hosts",
        ),
    ] {
        let err = sandbox.execute_with("true", &options).await.unwrap_err();
        assert!(err.to_string().contains(why), "{why}: {err}");
    }
    let offline = Sandbox::embedded()
        .shell()
        .with_command("tc", test_cmd())
        .build()
        .unwrap();
    let options = eryx::ExecuteOptions::new().with_credential(credential());
    let err = offline.execute_with("true", &options).await.unwrap_err();
    assert!(err.to_string().contains("need a child network"), "{err}");
}

/// The git tests' repositories: a bare `repo.git` under `<dir>/repos` with
/// two commits on `main` and one more on `feature`, made with the host's git.
fn git_repos(dir: &std::path::Path) -> PathBuf {
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    let repos = dir.join("repos");
    let work = dir.join("work");
    std::fs::create_dir_all(&repos).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&repos, &["init", "-q", "--bare", "repo.git"]);
    git(
        &repos.join("repo.git"),
        &["config", "http.receivepack", "true"],
    );
    git(&work, &["init", "-q"]);
    for (i, branch) in [(1, "main"), (2, "main"), (3, "feature")] {
        if i == 3 {
            git(&work, &["checkout", "-q", "-b", branch]);
        }
        std::fs::write(work.join("f.txt"), format!("line {i}\n")).unwrap();
        git(&work, &["add", "f.txt"]);
        let message = if i == 3 {
            "feature commit".to_owned()
        } else {
            format!("commit {i}")
        };
        git(&work, &["commit", "-q", "-m", &message]);
    }
    let bare = repos.join("repo.git");
    let bare = bare.to_str().unwrap();
    git(&work, &["push", "-q", bare, "main", "feature"]);
    repos
}

/// Whether a request's `Authorization` carries the real secret, as a bearer
/// token or `x-access-token:<secret>` Basic credentials (the proxy puts it
/// there in place of the placeholder the child sent).
fn authorized(authorization: Option<&str>) -> bool {
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{REAL}"));
    authorization == Some(&format!("Bearer {REAL}"))
        || authorization == Some(&format!("Basic {basic}"))
}

/// A smart-HTTP git server on 127.0.0.1: the host's `git http-backend` as
/// CGI over the bare repositories in `root`. Requests without the real
/// secret (see [`authorized`]) get a 401 asking for Basic credentials.
async fn git_server(root: PathBuf) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve_git(stream, root.clone()));
        }
    });
    port
}

/// Serve one connection's requests (keep-alive) for [`git_server`].
async fn serve_git(mut stream: tokio::net::TcpStream, root: PathBuf) {
    let mut buf = Vec::new();
    loop {
        // The request's head.
        let end = loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break end;
            }
            let mut chunk = [0u8; 8192];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end + 4);
        let mut lines = head.split("\r\n");
        let mut request = lines.next().unwrap_or_default().split(' ');
        let (method, target) = (
            request.next().unwrap_or_default(),
            request.next().unwrap_or_default(),
        );
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };

        if header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        // Its body: a length, or chunks.
        let mut more = async |buf: &mut Vec<u8>| {
            let mut chunk = [0u8; 8192];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => false,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    true
                }
            }
        };
        let mut body = Vec::new();
        if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
            while buf.len() < len {
                if !more(&mut buf).await {
                    return;
                }
            }
            body = buf.drain(..len).collect();
        } else if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            loop {
                let line_end = loop {
                    if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                        break i;
                    }
                    if !more(&mut buf).await {
                        return;
                    }
                };
                let size =
                    usize::from_str_radix(String::from_utf8_lossy(&buf[..line_end]).trim(), 16)
                        .unwrap_or(0);
                buf.drain(..line_end + 2);
                while buf.len() < size + 2 {
                    if !more(&mut buf).await {
                        return;
                    }
                }
                body.extend(buf.drain(..size));
                buf.drain(..2);
                if size == 0 {
                    break;
                }
            }
        }

        if !authorized(header("authorization")) {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
                      Content-Length: 0\r\n\r\n",
                )
                .await;
            continue;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut cgi = std::process::Command::new("git");
        cgi.arg("http-backend")
            .env("GIT_PROJECT_ROOT", &root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("REQUEST_METHOD", method)
            .env("PATH_INFO", path)
            .env("QUERY_STRING", query)
            .env("CONTENT_TYPE", header("content-type").unwrap_or_default())
            .env("CONTENT_LENGTH", body.len().to_string())
            .env("GIT_PROTOCOL", header("git-protocol").unwrap_or_default())
            .env(
                "HTTP_CONTENT_ENCODING",
                header("content-encoding").unwrap_or_default(),
            )
            .env("REMOTE_USER", "test")
            .env("REMOTE_ADDR", "127.0.0.1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let out = tokio::task::spawn_blocking(move || {
            let mut child = cgi.spawn().unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), &body).unwrap();
            child.wait_with_output().unwrap().stdout
        })
        .await
        .unwrap();
        let split = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(out.len());
        let (cgi_head, cgi_body) = (&out[..split], out.get(split + 4..).unwrap_or_default());
        let mut status = "200 OK".to_owned();
        let mut response = String::new();
        for line in String::from_utf8_lossy(cgi_head).split("\r\n") {
            match line.split_once(':') {
                Some((k, v)) if k.eq_ignore_ascii_case("status") => status = v.trim().to_owned(),
                Some(_) => response.push_str(&format!("{line}\r\n")),
                None => {}
            }
        }
        let head = format!(
            "HTTP/1.1 {status}\r\n{response}Content-Length: {}\r\n\r\n",
            cgi_body.len()
        );
        if stream.write_all(head.as_bytes()).await.is_err()
            || stream.write_all(cgi_body).await.is_err()
        {
            return;
        }
    }
}

/// A shell whose `git` (ERYX_TEST_GIT_WASM: git with HTTPS) may spawn its
/// helpers, with the secret TOKEN for `host` and a child network allowing
/// `network`'s hosts.
fn git_shell(network: NetConfig, host: &str, scrub_files: bool) -> Sandbox {
    let git = std::env::var_os("ERYX_TEST_GIT_WASM").expect("ERYX_TEST_GIT_WASM");
    Sandbox::embedded()
        .shell()
        .with_command("git", Command::from_file(git).unwrap().allow_spawn())
        .with_network(network)
        .with_secret("TOKEN", REAL, vec![host.to_string()])
        .scrub_files(scrub_files)
        .with_child_network(ChildNetwork::Inherit)
        .with_trace_collection(true)
        .build()
        .unwrap()
}

/// git clones, fetches and pushes over smart HTTP through the child proxy,
/// authenticating with a secret it only has as a placeholder: as a bearer
/// token in `http.extraHeader`, and as Basic credentials (which the proxy
/// substitutes inside the encoding) in the URL and from `credential.helper
/// store`, a helper git runs as a command. The server accepts only the
/// real secret, which never appears in the script's output or traces.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ERYX_TEST_GIT_WASM (git with HTTPS) and the host's git"]
async fn git_clones_fetches_and_pushes_over_http_with_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let repos = git_repos(dir.path());
    let port = git_server(repos.clone()).await;
    // git keeps these credentials in files (config, the remote URL in
    // .git/config, the credential store); with the VFS, file scrubbing would
    // replace the placeholder there before git sends it.
    let sandbox = git_shell(git_test_network(), "git.test", false);
    let url = format!("http://git.test:{port}/repo.git");
    let script = format!(
        r#"export HOME=/tmp GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
cd /tmp
git config --global http.extraHeader "Authorization: Bearer $TOKEN"
git clone -q {url} c && git -C c log --format=%s
git -C c fetch -q origin feature && git -C c log -1 --format=%s FETCH_HEAD
cd c && git config user.name eryx && git config user.email eryx@example.com
echo pushed >> f.txt && git commit -qam 'pushed from eryx' && git push -q origin HEAD:refs/heads/pushed && echo pushed
cd /tmp && git config --global --unset http.extraHeader
git clone -q http://x-access-token:$TOKEN@git.test:{port}/repo.git b && echo basic-clone
printf 'http://x-access-token:%s@git.test:{port}\n' "$TOKEN" > /tmp/credentials
git -c "credential.helper=store --file=/tmp/credentials" clone -q {url} s && echo store-clone
git ls-remote {url} >/dev/null 2>&1 || echo refused-without-token
"#
    );
    let out = match sandbox.execute(&script).await {
        Ok(out) => out,
        Err(e) => panic!("{e}"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout,
        "commit 2\ncommit 1\nfeature commit\npushed\nbasic-clone\nstore-clone\n\
         refused-without-token\n",
        "{stderr}"
    );
    let pushed = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", "pushed"])
        .current_dir(repos.join("repo.git"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&pushed.stdout),
        "pushed from eryx\n"
    );
    for (what, text) in [
        ("stdout", stdout.into_owned()),
        ("stderr", stderr.into_owned()),
        ("traces", format!("{:?}", out.trace)),
    ] {
        assert!(!text.contains(REAL), "the secret is in the {what}");
    }
}

/// With file scrubbing on, git still authenticates when the placeholder only
/// reaches it on the command line or in the environment, never in a file.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ERYX_TEST_GIT_WASM (git with HTTPS) and the host's git"]
async fn git_authenticates_without_writing_the_secret_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let port = git_server(git_repos(dir.path())).await;
    let sandbox = git_shell(git_test_network(), "git.test", true);
    let url = format!("http://git.test:{port}/repo.git");
    let out = run(
        &sandbox,
        &format!(
            r#"export HOME=/tmp GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
cd /tmp
git -c "http.extraHeader=Authorization: Bearer $TOKEN" clone -q {url} c && git -C c log -1 --format=%s
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="Authorization: Bearer $TOKEN" git -C c fetch -q origin feature && echo env-fetch
"#
        ),
    )
    .await;
    assert!(out.starts_with("commit 2\nenv-fetch\n"), "{out}");
    assert!(!out.contains(REAL));
}

/// git clones, fetches and pushes with a credential only the proxy holds:
/// no token, placeholder or credential config in the sandbox, which can't
/// find it afterwards in git's files, its env or any output. Another
/// command's requests to the same server don't get it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ERYX_TEST_GIT_WASM (git with HTTPS) and the host's git"]
async fn git_authenticates_with_a_credential_the_sandbox_never_sees() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let repos = git_repos(dir.path());
    let port = git_server(repos.clone()).await;
    let git = std::env::var_os("ERYX_TEST_GIT_WASM").expect("ERYX_TEST_GIT_WASM");
    let sandbox = Sandbox::embedded()
        .shell()
        .with_command("git", Command::from_file(git).unwrap().allow_spawn())
        .with_command("tc", test_cmd())
        .with_network(git_test_network())
        .with_child_network(ChildNetwork::Inherit)
        .with_credential(
            eryx::Credential::basic("x-access-token", REAL)
                .for_hosts(["git.test"])
                .for_commands(["git"])
                .allow_plaintext(),
        )
        .with_trace_collection(true)
        .build()
        .unwrap();
    let url = format!("http://git.test:{port}/repo.git");
    let script = format!(
        r#"export HOME=/tmp GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
cd /tmp
git clone -q {url} c && git -C c log --format=%s
git -C c fetch -q origin feature && git -C c log -1 --format=%s FETCH_HEAD
cd c && git config user.name eryx && git config user.email eryx@example.com
echo pushed >> f.txt && git commit -qam 'pushed from eryx' && git push -q origin HEAD:refs/heads/pushed && echo pushed
tc proxy-get {url}/info/refs?service=git-upload-pack | head -1
cat /tmp/c/.git/config; env; git config --list
"#
    );
    let out = match sandbox.execute(&script).await {
        Ok(out) => out,
        Err(e) => panic!("{e}"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.starts_with("commit 2\ncommit 1\nfeature commit\npushed\nHTTP/1.1 401 "),
        "{stdout}\n{stderr}"
    );
    let pushed = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", "pushed"])
        .current_dir(repos.join("repo.git"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&pushed.stdout),
        "pushed from eryx\n"
    );
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{REAL}"));
    for (what, text) in [
        ("stdout", stdout.into_owned()),
        ("stderr", stderr.into_owned()),
        ("traces", format!("{:?}", out.trace)),
    ] {
        assert!(
            !text.contains(REAL) && !text.contains(&basic),
            "the credential is in the {what}"
        );
    }
}

/// A network reaching the local git server as `git.test`.
fn git_test_network() -> NetConfig {
    NetConfig::default()
        .allow_host("git.test")
        .allow_internal_host("git.test")
        .with_resolver(Localhost)
        .allow_plaintext_secrets(true)
}

/// git clones a public repository from GitHub over HTTPS, through the child
/// proxy's TLS, trusting its CA from where children find it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ERYX_TEST_GIT_WASM (git with HTTPS) and the network"]
async fn git_clones_from_github_over_https() {
    let sandbox = git_shell(
        NetConfig::default().allow_host("github.com"),
        "github.com",
        true,
    );
    let out = run(
        &sandbox,
        "export HOME=/tmp GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0; cd /tmp; \
         git clone -q https://github.com/octocat/Hello-World.git hw && git -C hw log -1 --format=%s",
    )
    .await;
    assert!(out.contains("Merge pull request #6"), "{out}");
}
