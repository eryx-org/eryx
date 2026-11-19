//! Hashes of each guest's sources, which `Executor::stamp_language` writes
//! into a precompiled runtime and loading checks: a runtime built from other
//! guest sources fails with `IncompatiblePrecompiled` even when the eryx
//! version is the same, as in development builds.
//!
//! Outside a git checkout (the crates.io package) there are no guest sources,
//! so nothing is set and nothing is checked.

use std::path::{Path, PathBuf};

/// What each guest is built from, relative to the workspace root: the `rustc`
/// env var its hash goes in, and its sources. Python's list is eryx-runtime's
/// `GUEST_SOURCES`; the others are their mise build tasks' `sources`.
// ponytail: copies of those lists; a file added to a guest but not here just
// isn't hashed, so a stale runtime built only from it isn't caught.
const GUESTS: [(&str, &[&str]); 3] = [
    (
        "ERYX_GUEST_HASH_PYTHON",
        &[
            "crates/eryx-guest-python/src",
            "crates/eryx-guest-python/clock_stubs.c",
            "crates/eryx-guest-python/Cargo.toml",
            "crates/eryx-guest-call/src",
            "crates/eryx-guest-call/Cargo.toml",
            "crates/wit-dylib-ffi/src",
            "crates/wit-dylib-ffi/Cargo.toml",
            "crates/eryx-runtime/wit",
        ],
    ),
    (
        "ERYX_GUEST_HASH_JAVASCRIPT",
        &[
            "crates/eryx-guest-js/src",
            "crates/eryx-guest-js/Cargo.toml",
            "crates/eryx-guest-call/src",
            "crates/eryx-guest-call/Cargo.toml",
            "crates/eryx-guest-python/clock_stubs.c",
            "crates/wit-dylib-ffi/src",
            "crates/wit-dylib-ffi/Cargo.toml",
            "crates/eryx-runtime/wit",
        ],
    ),
    (
        "ERYX_GUEST_HASH_SHELL",
        &[
            "crates/eryx-guest-shell/src",
            // Not the whole wit/ dir: its other deps are symlinks into
            // eryx-runtime/wit, below, which a Windows checkout makes text
            // files with other contents.
            "crates/eryx-guest-shell/wit/shell.wit",
            "crates/eryx-guest-shell/wit/deps/process.wit",
            "crates/eryx-guest-shell/Cargo.toml",
            "crates/eryx-guest-shell/Cargo.lock",
            "crates/eryx-runtime/wit",
        ],
    ),
];

/// Set each guest's hash, if this is a checkout.
pub fn prepare() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.parent().unwrap().parent().unwrap();
    for (var, sources) in GUESTS {
        if !root.join(sources[0]).exists() {
            continue;
        }
        let mut files = Vec::new();
        for source in sources {
            let path = root.join(source);
            println!("cargo::rerun-if-changed={}", path.display());
            collect_files(&path, &mut files);
        }
        files.sort();
        println!("cargo::rustc-env={var}={:016x}", hash(root, &files));
    }
}

/// FNV-1a over each file's `/`-separated path and contents, skipping carriage
/// returns: the same on every platform and toolchain (eryx-runtime's
/// `guest_sources_hash` does the same).
fn hash(root: &Path, files: &[PathBuf]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes.iter().filter(|&&b| b != b'\r') {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash ^= 0xff;
    };
    for file in files {
        let relative = file.strip_prefix(root).unwrap();
        let components: Vec<_> = relative.iter().map(|c| c.to_string_lossy()).collect();
        feed(components.join("/").as_bytes());
        feed(&std::fs::read(file).expect("failed to read guest source"));
    }
    hash
}

/// Push `path` if it is a file, or every file under it if it is a directory.
///
/// Panics at a symlink: a Windows checkout makes one a text file holding its
/// target, so the hash would differ there and refuse runtimes built on Linux.
/// List the target instead.
fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_symlink() {
        panic!(
            "{} is a symlink, which hashes differently on Windows: \
             list what it points to in build/guest_sources.rs instead",
            path.display()
        );
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path).expect("failed to read guest source dir") {
            collect_files(&entry.expect("failed to read dir entry").path(), files);
        }
    } else if path.exists() {
        files.push(path.to_path_buf());
    }
}
