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
//!   after each;
//! - `alloc MIB`: hold MIB MiB of memory while copying stdin to stdout, or
//!   print `error: OutOfMemory` to stderr and exit 1.
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
            while stdout.write_all(b"y\n").and_then(|()| stdout.flush()).is_ok() {}
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
        ["alloc", mib] => {
            let mut held: Vec<u8> = Vec::new();
            let size = mib.parse::<usize>().unwrap_or(0) << 20;
            if held.try_reserve_exact(size).is_err() {
                eprintln!("error: OutOfMemory");
                std::process::exit(1);
            }
            held.resize(size, 1);
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout())
                .map(|_| String::from(if held[size / 2] == 1 { "" } else { "?" }))
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
