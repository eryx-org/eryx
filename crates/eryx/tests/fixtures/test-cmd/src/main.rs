//! A test command for eryx's `eryx:process`. What it does depends on its first
//! argument:
//!
//! - none: copy stdin to stdout, upper-cased;
//! - `argv0`: print `argv[0]`;
//! - `env NAME`: print `$NAME`;
//! - `cat PATH`, `write PATH TEXT`: read or write a file;
//! - `exit N`: exit with status `N`, after printing `bye` to stderr;
//! - `spin`: loop forever;
//! - `yes`: print `y` lines until stdout is closed (`lines`: one write
//!   each);
//! - `burn PATH N`: N rounds of busy work, writing the rounds done to PATH
//!   after each (`busy N`: the same, with no calls at all);
//! - `alloc MIB`: hold MIB MiB of memory while copying stdin to stdout, or
//!   print `error: OutOfMemory` to stderr and exit 1;
//! - `connect ADDR`: open a TCP connection to `ADDR` and print `connected`;
//! - `proxy-get URL`: `GET URL` through `$HTTP_PROXY`, with
//!   `Authorization: Bearer $TOKEN`, printing the response;
//! - `tunnel-get HOST:PORT`: the same request, in plain HTTP inside a
//!   `CONNECT HOST:PORT` to `$HTTPS_PROXY`;
//! - `hold N`: open up to N connections to `$HTTP_PROXY` and keep them,
//!   printing `held <count>` and the error that stopped it, if any;
//! - `listen`: listen on a TCP port and print `listening`.
//!
//! A failure prints `error: <io::ErrorKind>` and exits 1.

use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let result = match rest.as_slice() {
        [] => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map(|_| input.to_uppercase())
        }
        ["argv0"] => Ok(format!("{}\n", args[0])),
        ["env", name] => Ok(format!("{}\n", std::env::var(name).unwrap_or_default())),
        ["cat", path] => std::fs::read_to_string(path),
        ["write", path, text] => std::fs::write(path, text).map(|()| String::new()),
        ["yes"] => {
            let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
            while stdout.write_all(b"y\n").is_ok() {}
            std::process::exit(0);
        }
        ["lines"] => {
            // One write per line, as a line-buffered program makes.
            let mut stdout = std::io::stdout().lock();
            while stdout
                .write_all(b"y\n")
                .and_then(|()| stdout.flush())
                .is_ok()
            {}
            std::process::exit(0);
        }
        ["burn", path, rounds] => {
            for round in 1..=rounds.parse().unwrap_or(0u32) {
                let mut x = 0u64;
                for i in 0..50_000_000u64 {
                    x = std::hint::black_box(x.wrapping_add(i));
                }
                if std::fs::write(path, round.to_string()).is_err() {
                    std::process::exit(1);
                }
            }
            Ok(String::new())
        }
        ["busy", rounds] => {
            let mut x = 0u64;
            for i in 0..u64::from(rounds.parse().unwrap_or(0u32)) * 50_000_000 {
                x = std::hint::black_box(x.wrapping_add(i));
            }
            Ok(String::new())
        }
        ["alloc", mib] => {
            let mut held: Vec<u8> = Vec::new();
            let size = mib.parse::<usize>().unwrap_or(0) << 20;
            if held.try_reserve_exact(size).is_err() {
                eprintln!("error: OutOfMemory");
                std::process::exit(1);
            }
            held.resize(size, 1);
            let copied = std::io::copy(&mut std::io::stdin(), &mut std::io::stdout());
            std::hint::black_box(&held);
            copied.map(|_| String::new())
        }
        ["connect", addr] => std::net::TcpStream::connect(addr).map(|_| "connected\n".to_string()),
        ["proxy-get", url] => proxy_get(url),
        ["tunnel-get", authority] => tunnel_get(authority),
        ["hold", n] => {
            let mut held = Vec::new();
            let mut error = String::new();
            for _ in 0..n.parse().unwrap_or(0u32) {
                match proxy_addr().and_then(std::net::TcpStream::connect) {
                    Ok(stream) => held.push(stream),
                    Err(e) => {
                        error = format!(" error: {:?}", e.kind());
                        break;
                    }
                }
            }
            Ok(format!("held {}{error}\n", held.len()))
        }
        ["listen"] => std::net::TcpListener::bind("127.0.0.1:0").map(|_| "listening\n".to_string()),
        ["spin"] => loop {
            std::hint::spin_loop();
        },
        ["exit", code] => {
            eprintln!("bye");
            std::process::exit(code.parse().unwrap_or(2));
        }
        _ => {
            eprintln!("usage: see src/main.rs");
            std::process::exit(2);
        }
    };
    match result {
        Ok(out) => {
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
        Err(e) => {
            println!("error: {:?}", e.kind());
            std::process::exit(1);
        }
    }
}

/// `GET url` through the plain-HTTP proxy in `$HTTP_PROXY`.
fn proxy_get(url: &str) -> std::io::Result<String> {
    let proxy = std::env::var("HTTP_PROXY").unwrap_or_default();
    let proxy = proxy.trim_start_matches("http://").trim_end_matches('/');
    let host = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    let token = std::env::var("TOKEN").unwrap_or_default();
    let mut stream = std::net::TcpStream::connect(
        proxy
            .parse::<std::net::SocketAddr>()
            .map_err(|_| std::io::ErrorKind::InvalidInput)?,
    )?;
    write!(
        stream,
        "GET {url} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

/// The proxy's address from `$HTTP_PROXY` (children have no name lookup).
fn proxy_addr() -> std::io::Result<std::net::SocketAddr> {
    let proxy = std::env::var("HTTP_PROXY").unwrap_or_default();
    proxy
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .parse()
        .map_err(|_| std::io::ErrorKind::InvalidInput.into())
}

/// A plain-HTTP `GET /` inside `CONNECT authority` to `$HTTPS_PROXY`,
/// printing both responses.
fn tunnel_get(authority: &str) -> std::io::Result<String> {
    let host = authority.split(':').next().unwrap_or_default();
    let token = std::env::var("TOKEN").unwrap_or_default();
    let mut stream = std::net::TcpStream::connect(proxy_addr()?)?;
    write!(
        stream,
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
    )?;
    // The proxy's reply to the CONNECT, up to its blank line.
    let mut reply = Vec::new();
    let mut byte = [0u8];
    while !reply.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
        reply.push(byte[0]);
    }
    let mut response = String::from_utf8_lossy(&reply).into_owned();
    if response.contains(" 200 ") {
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        )?;
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        response.push_str(&String::from_utf8_lossy(&rest));
    }
    Ok(response)
}
