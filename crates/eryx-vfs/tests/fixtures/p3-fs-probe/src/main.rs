//! Runs one filesystem operation from its arguments and prints the result.
//!
//! - `write PATH TEXT`, `append PATH TEXT`: print `ok`.
//! - `read PATH`: print the contents.
//! - `ls PATH`: print the sorted entry names, one per line.
//! - `mkdir PATH`, `rm PATH`, `rename FROM TO`: print `ok`.
//! - `fill PATH BYTES`: write BYTES bytes in 64 KiB chunks, then print `ok`.
//! - `excl PATH`: create PATH with `O_CREAT|O_EXCL`, then print `ok`.
//! - `appendn PATH TAG N`: append N lines `TAG-<i>`, one write each, then
//!   print `ok`.
//!
//! A failure prints `error: <io::ErrorKind>` and exits 1.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["write", path, text] => std::fs::write(path, text).map(|()| "ok".to_string()),
        ["append", path, text] => std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map(|()| "ok".to_string()),
        ["read", path] => std::fs::read_to_string(path),
        ["ls", path] => std::fs::read_dir(path).and_then(|entries| {
            let mut names = entries
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<Result<Vec<_>, _>>()?;
            names.sort();
            Ok(names.join("\n"))
        }),
        ["mkdir", path] => std::fs::create_dir(path).map(|()| "ok".to_string()),
        ["rm", path] => std::fs::remove_file(path).map(|()| "ok".to_string()),
        ["rename", from, to] => std::fs::rename(from, to).map(|()| "ok".to_string()),
        ["excl", path] => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| "ok".to_string()),
        ["appendn", path, tag, n] => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| {
                for i in 0..n.parse::<usize>().unwrap_or(0) {
                    f.write_all(format!("{tag}-{i:03}\n").as_bytes())?;
                }
                Ok("ok".to_string())
            }),
        ["fill", path, bytes] => fill(path, bytes.parse().unwrap_or(0)),
        _ => {
            eprintln!("usage: see src/main.rs");
            std::process::exit(2);
        }
    };
    match result {
        Ok(out) => println!("{out}"),
        Err(err) => {
            println!("error: {:?}", err.kind());
            std::process::exit(1);
        }
    }
}

fn fill(path: &str, bytes: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::create(path)?;
    let chunk = vec![b'x'; 64 * 1024];
    let mut left = bytes;
    while left > 0 {
        let n = left.min(chunk.len());
        file.write_all(&chunk[..n])?;
        left -= n;
    }
    file.flush()?;
    Ok("ok".to_string())
}
