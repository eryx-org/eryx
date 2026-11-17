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
//! - `listen`: listen on a TCP port and print `listening`;
//! - `spawn CMD ARG...`: run `CMD` through `eryx:process/run` with this
//!   command's stdin, env and `$PWD`, passing on its output and exit status,
//!   or printing `error: <run-error>` (e.g. `limit-exceeded`) and exiting 1.
//! - `copy`: copy stdin to stdout upper-cased as it arrives, a chunk at a
//!   time (no-args mode reads all of stdin first);
//! - `pipe CMD ARG...`: run `CMD` through `eryx:process/pipe`, writing all
//!   of this command's stdin to it, then printing its stdout and exiting
//!   with its status (`error: <run-error>` and 1 if it fails);
//! - `pipe-echo N CMD ARG...`: write N bytes to a piped `CMD` (which echoes
//!   them, as `copy` does) 4 KiB at a time, reading each chunk back before
//!   the next, and print `echoed <bytes>`;
//! - `pipe-flood N CMD ARG...`: write N bytes to a piped `CMD` without ever
//!   reading its output, then wait for it and print `done`;
//! - `pipe-leak N CMD ARG...`: pipe up to N `CMD`s and exit without waiting
//!   for them, printing `piped <count>` and the error that stopped it.
//!
//! A failure prints `error: <io::ErrorKind>` and exits 1.

use std::io::{Read, Write};

wit_bindgen::generate!({
    path: "../../../../eryx-runtime/wit/deps/process.wit",
    world: "eryx:process/child",
});

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
        ["spawn", cmd, args @ ..] => {
            let mut input = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut input);
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            let env: Vec<(String, String)> = std::env::vars().collect();
            let cwd = std::env::var("PWD").unwrap_or_else(|_| "/".to_owned());
            match eryx::process::run::run(cmd, &args, &env, &cwd, &input) {
                Ok(out) => {
                    let _ = std::io::stdout().write_all(&out.stdout);
                    let _ = std::io::stderr().write_all(&out.stderr);
                    let _ = std::io::stdout().flush();
                    std::process::exit(out.status);
                }
                Err(e) => {
                    println!("error: {}", e.name());
                    std::process::exit(1);
                }
            }
        }
        ["copy"] => {
            let mut buf = [0u8; 4096];
            let mut stdout = std::io::stdout().lock();
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(n) => {
                        buf[..n].make_ascii_uppercase();
                        if stdout.write_all(&buf[..n]).and_then(|()| stdout.flush()).is_err() {
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        ["pipe", cmd, args @ ..] => {
            let child = piped(cmd, args);
            let mut input = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut input);
            if !input.is_empty() {
                let _ = child.write(&input);
            }
            child.close_stdin();
            let mut stdout = std::io::stdout().lock();
            loop {
                match child.read(64 * 1024) {
                    Ok(chunk) if !chunk.is_empty() => {
                        let _ = stdout.write_all(&chunk);
                    }
                    _ => break,
                }
            }
            let _ = stdout.flush();
            match child.wait() {
                Ok(status) => std::process::exit(status),
                Err(e) => {
                    println!("error: {}", e.name());
                    std::process::exit(1);
                }
            }
        }
        ["pipe-echo", n, cmd, args @ ..] => {
            let child = piped(cmd, args);
            let total: usize = n.parse().unwrap_or(0);
            let chunk = vec![b'x'; 4096];
            let mut echoed = 0;
            while echoed < total {
                let len = chunk.len().min(total - echoed);
                if child.write(&chunk[..len]).is_err() {
                    break;
                }
                let mut got = 0;
                while got < len {
                    match child.read((len - got) as u64) {
                        Ok(back) if !back.is_empty() => {
                            assert!(back.iter().all(|&b| b == b'X'), "copy upper-cases");
                            got += back.len();
                        }
                        _ => break,
                    }
                }
                echoed += got;
                if got < len {
                    break;
                }
            }
            child.close_stdin();
            let _ = child.wait();
            Ok(format!("echoed {echoed}\n"))
        }
        ["pipe-flood", n, cmd, args @ ..] => {
            let child = piped(cmd, args);
            let _ = child.write(&vec![b'x'; n.parse().unwrap_or(0)]);
            let _ = child.wait();
            Ok("done\n".to_owned())
        }
        ["pipe-leak", n, cmd, args @ ..] => {
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            let env: Vec<(String, String)> = std::env::vars().collect();
            let cwd = std::env::var("PWD").unwrap_or_else(|_| "/".to_owned());
            let mut held = Vec::new();
            let mut stopped = String::new();
            for _ in 0..n.parse().unwrap_or(0) {
                match eryx::process::pipe::Piped::spawn(cmd, &args, &env, &cwd) {
                    Ok(child) => held.push(child),
                    Err(e) => {
                        stopped = format!(" error: {}", e.name());
                        break;
                    }
                }
            }
            println!("piped {}{stopped}", held.len());
            let _ = std::io::stdout().flush();
            // Exiting drops them, which kills them.
            std::process::exit(0);
        }
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

/// `cmd args` started through `eryx:process/pipe` with this command's env
/// and `$PWD`; on failure, print `error: <run-error>` and exit 1.
fn piped(cmd: &str, args: &[&str]) -> eryx::process::pipe::Piped {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = std::env::var("PWD").unwrap_or_else(|_| "/".to_owned());
    match eryx::process::pipe::Piped::spawn(cmd, &args, &env, &cwd) {
        Ok(child) => child,
        Err(e) => {
            println!("error: {}", e.name());
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
