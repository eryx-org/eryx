//! `fetch` in the JavaScript guest (#825), against a local HTTPS server.
//!
//! Requests go through the host's `eryx:net` path: `api.test` resolves to the
//! local server through a [`Resolver`], and its certificate is trusted with
//! `with_root_cert`, so the host's policy, HTTP rules and secret
//! substitution all apply as they would to a real host.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_fetch
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eryx::language::JavaScript;
use eryx::{HttpRule, IpNet, NetConfig, Resolver, Sandbox, SandboxBuilder, state};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const HOST: &str = "api.test";
const SECRET: &str = "real-secret-value-12345";

/// Points [`HOST`] at the test server.
#[derive(Debug)]
struct Pinned(SocketAddr);

#[async_trait::async_trait]
impl Resolver for Pinned {
    async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        if host == HOST {
            Ok(vec![self.0])
        } else {
            Err(std::io::Error::other(format!("unknown host {host}")))
        }
    }
}

/// An HTTPS server for [`HOST`] that records each request and answers by path.
struct Server {
    addr: SocketAddr,
    cert: Vec<u8>,
    requests: Arc<Mutex<Vec<String>>>,
}

async fn respond(path: &str, request: &str) -> Vec<u8> {
    let plain = |status: &str, headers: &str, body: &str| {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    match path {
        "/json" => plain(
            "200 OK",
            "Content-Type: application/json\r\nX-Multi: a\r\nX-Multi: b\r\n",
            r#"{"hello":"world"}"#,
        ),
        "/echo" => plain("200 OK", "", request),
        "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
            6\r\nchunk-\r\n4\r\ned!!\r\n0\r\n\r\n"
            .to_vec(),
        "/redirect" => plain("302 Found", "Location: /json\r\n", ""),
        "/see-other" => plain("303 See Other", "Location: /echo\r\n", ""),
        "/missing" => plain("404 Not Found", "", "nope"),
        "/stall" => {
            tokio::time::sleep(Duration::from_secs(10)).await;
            plain("200 OK", "", "late")
        }
        _ => plain("500 Internal Server Error", "", "unknown path"),
    }
}

impl Server {
    async fn start() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec![HOST.to_string()]).unwrap();
        let cert_der = cert.cert.der().to_vec();
        let key = PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert_der.clone())], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    // Read the head, then as much body as it declares.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match tls.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    while buf.len() < head_end + length {
                        match tls.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&buf).into_owned();
                    recorded.lock().unwrap().push(request.clone());
                    let path = request.split(' ').nth(1).unwrap_or("/").to_string();
                    let response = respond(&path, &request).await;
                    let _ = tls.write_all(&response).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            addr,
            cert: cert_der,
            requests,
        }
    }

    fn net(&self) -> NetConfig {
        NetConfig::default()
            .allow_host(HOST)
            .allow_network("127.0.0.0/8".parse::<IpNet>().unwrap())
            .with_resolver(Pinned(self.addr))
            .with_root_cert(self.cert.clone())
    }

    fn sandbox(&self, net: NetConfig) -> SandboxBuilder<JavaScript, state::Has> {
        Sandbox::embedded().javascript().with_network(net)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn run(sandbox: &eryx::Sandbox, code: &str) -> String {
    match sandbox.execute(code).await {
        Ok(out) => out.stdout_text(),
        Err(e) => panic!("{e}\nstderr: {:?}", e.stderr().map(String::from_utf8_lossy)),
    }
}

#[tokio::test]
async fn get_json_headers_and_status() {
    let server = Server::start().await;
    let sandbox = server.sandbox(server.net()).build().unwrap();
    let out = run(
        &sandbox,
        r"
        const res = await fetch('https://api.test/json');
        console.log(res.status, res.ok, res.statusText, res.url, res.redirected);
        console.log(res.headers.get('content-type'), res.headers.get('X-Multi'));
        console.log(JSON.stringify(await res.json()), res.bodyUsed);
        const missing = await fetch('https://api.test/missing');
        console.log(missing.status, missing.ok, await missing.text());
        ",
    )
    .await;
    assert_eq!(
        out,
        "200 true OK https://api.test/json false\n\
         application/json a, b\n\
         {\"hello\":\"world\"} true\n\
         404 false nope\n"
    );
}

#[tokio::test]
async fn post_body_chunked_response_and_redirects() {
    let server = Server::start().await;
    let sandbox = server.sandbox(server.net()).build().unwrap();
    let out = run(
        &sandbox,
        r#"
        const echo = await fetch(new Request('https://api.test/echo', {
            method: 'post',
            headers: { 'X-Custom': 'yes' },
            body: JSON.stringify({ n: 1 }),
        }));
        const sent = await echo.text();
        console.log(sent.startsWith('POST /echo HTTP/1.1\r\n'), sent.includes('x-custom: yes'),
            sent.includes('content-type: text/plain;charset=UTF-8'), sent.endsWith('{"n":1}'));
        console.log(await (await fetch('https://api.test/chunked')).text());
        const followed = await fetch('https://api.test/redirect');
        console.log(followed.status, followed.redirected, followed.url);
        const manual = await fetch('https://api.test/redirect', { redirect: 'manual' });
        console.log(manual.status, manual.headers.get('location'));
        const seeOther = await fetch('https://api.test/see-other', { method: 'POST', body: 'x' });
        console.log((await seeOther.text()).split('\r\n')[0]);
        try {
            await fetch('https://api.test/redirect', { redirect: 'error' });
        } catch (e) {
            console.log(e.name, e.message, e.cause.code);
        }
        "#,
    )
    .await;
    assert_eq!(
        out,
        "true true true true\n\
         chunk-ed!!\n\
         200 true https://api.test/json\n\
         302 /json\n\
         GET /echo HTTP/1.1\n\
         TypeError fetch failed redirect\n"
    );
}

/// A script reads a secret's placeholder from `process.env`; the host sends
/// the real value, puts the placeholder back in what the server echoes, and
/// scrubs it from stdout.
#[tokio::test]
async fn secret_placeholder_substituted_in_authorization() {
    let server = Server::start().await;
    let sandbox = server
        .sandbox(server.net())
        .with_secret("API_TOKEN", SECRET, vec![HOST.to_string()])
        .with_secret("__proto__", "other-secret-value", vec![HOST.to_string()])
        .build()
        .unwrap();
    let out = run(
        &sandbox,
        r"
        const token = process.env.API_TOKEN;
        console.log(Object.keys(globalThis).includes('process'), token === 'real-secret-value-12345');
        console.log(Object.hasOwn(process.env, '__proto__'), Object.getPrototypeOf(process.env) === Object.prototype);
        const res = await fetch('https://api.test/echo', {
            headers: { Authorization: `Bearer ${token}` },
        });
        const echoed = await res.text();
        console.log(echoed.includes(`authorization: Bearer ${token}`), echoed.includes('real-secret'));
        console.log(token);
        ",
    )
    .await;
    assert_eq!(out, "false false\ntrue true\ntrue false\n[REDACTED]\n");
    assert!(!out.contains(SECRET));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(&format!("authorization: Bearer {SECRET}\r\n")),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn http_rules_and_blocked_hosts_reject_with_not_permitted() {
    let server = Server::start().await;
    let net = server
        .net()
        .allow_http(HttpRule::new(HOST).with_methods(["GET"]));
    let sandbox = server.sandbox(net).build().unwrap();
    let out = run(
        &sandbox,
        r"
        const fail = async (...args) => {
            try {
                await fetch(...args);
                return 'no error';
            } catch (e) {
                return `${e.name}: ${e.message} (${e.cause?.code})`;
            }
        };
        console.log((await fetch('https://api.test/json')).status);
        console.log(await fail('https://api.test/echo', { method: 'DELETE' }));
        console.log(await fail('https://elsewhere.test/'));
        console.log(await fail('ftp://api.test/'));
        console.log(await fail('https://api.test/', { headers: { 'X-Bad': 'a\nb' } }));
        // JS-side checks can be patched away; the Rust side checks again.
        RegExp.prototype.test = () => true;
        console.log(await fail('https://api.test/json', { method: 'GET /json HTTP/1.1\r\nX-A: 1\r\n\r\nDELETE' }));
        ",
    )
    .await;
    assert_eq!(
        out,
        "200\n\
         TypeError: fetch failed (not-permitted)\n\
         TypeError: fetch failed (not-permitted)\n\
         TypeError: fetch only supports http and https, not ftp: (undefined)\n\
         TypeError: Invalid header value (undefined)\n\
         TypeError: invalid method \"GET /json HTTP/1.1\\r\\nX-A: 1\\r\\n\\r\\nDELETE\" (undefined)\n"
    );
    // The DELETE never reached the server.
    assert!(server.requests().iter().all(|r| !r.starts_with("DELETE")));
}

#[tokio::test]
async fn abort_signals() {
    let server = Server::start().await;
    let sandbox = server.sandbox(server.net()).build().unwrap();
    let start = std::time::Instant::now();
    let out = run(
        &sandbox,
        r"
        const controller = new AbortController();
        controller.abort();
        try {
            await fetch('https://api.test/json', { signal: controller.signal });
        } catch (e) {
            console.log(e.name);
        }
        try {
            await fetch('https://api.test/stall', { signal: AbortSignal.timeout(300) });
        } catch (e) {
            console.log(e.name, e.message);
        }
        ",
    )
    .await;
    assert_eq!(out, "AbortError\nTimeoutError The operation timed out.\n");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn fetch_in_a_session_with_secrets() {
    use eryx::Session as _;

    let server = Server::start().await;
    let sandbox = server
        .sandbox(server.net())
        .with_secret("API_TOKEN", SECRET, vec![HOST.to_string()])
        .build()
        .unwrap();
    let mut session = eryx::InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("var res = await fetch('https://api.test/json')")
        .await
        .unwrap();
    let out = session
        .execute(
            "await fetch('https://api.test/echo', { headers: { 'X-Key': process.env.API_TOKEN } });
             console.log((await res.json()).hello)",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"world\n");
    assert!(server.requests()[1].contains(&format!("x-key: {SECRET}\r\n")));
}
