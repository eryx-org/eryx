//! `https-get URL [HEADER...]`: `GET URL` (`https://host[:port]/path`)
//! through the `CONNECT` proxy in `$HTTPS_PROXY`, verifying the server
//! against the certificates in `$SSL_CERT_FILE` only, and print the
//! response. Each `HEADER` is a `Name: value` line to send.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match get(&args[0], &args[1..]) {
        Ok(response) => print!("{response}"),
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn get(url: &str, headers: &[String]) -> Result<String, Box<dyn std::error::Error>> {
    let rest = url.strip_prefix("https://").ok_or("not an https:// URL")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    let host = authority.split(':').next().unwrap_or_default();
    let authority = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{host}:443")
    };

    // The proxy is reached by address: children have no name lookup.
    let proxy = std::env::var("HTTPS_PROXY")?;
    let proxy = proxy.trim_start_matches("http://").trim_end_matches('/');
    let mut tcp = TcpStream::connect(proxy.parse::<std::net::SocketAddr>()?)?;
    write!(
        tcp,
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
    )?;
    let mut reader = BufReader::new(tcp);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.contains(" 200 ") {
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
        return Ok(format!("{status}{rest}"));
    }
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let tcp = reader.into_inner();

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(std::env::var("SSL_CERT_FILE")?)? {
        roots.add(cert?)?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(config), ServerName::try_from(host.to_string())?)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for header in headers {
        request.push_str(&format!("{header}\r\n"));
    }
    request.push_str("\r\n");
    tls.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    // A server closing without close_notify still sent the whole response.
    if let Err(e) = tls.read_to_end(&mut response)
        && e.kind() != std::io::ErrorKind::UnexpectedEof
    {
        return Err(e.into());
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}
