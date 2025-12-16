// Tests use expect/unwrap for simplicity
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Test to verify wit-dylib linking works with our runtime.
//!
//! Run with: cargo test --package eryx-wasm-runtime --test link_test
//!
//! This test links all required libraries (libc, libc++, libpython, WASI emulation libs)
//! together with our runtime to produce a valid WASM component.

use std::io::Cursor;
use std::path::PathBuf;
use wit_component::{Linker, StringEncoding, embed_component_metadata};

fn decompress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(Cursor::new(data)).expect("failed to decompress")
}

#[test]
fn test_link_runtime() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest_dir.parent().unwrap().parent().unwrap();
    let libs_dir = project_root.join("crates/eryx-runtime/libs");

    // Path to our runtime .so
    let runtime_path = manifest_dir.join("target/liberyx_runtime.so");
    if !runtime_path.exists() {
        panic!(
            "Runtime not found at {}. Run `BUILD_ERYX_RUNTIME=1 cargo build -p eryx-runtime` first",
            runtime_path.display()
        );
    }

    println!("Loading runtime from: {}", runtime_path.display());
    let runtime = std::fs::read(&runtime_path)?;
    println!("Runtime size: {} bytes", runtime.len());

    // Load all required libraries (zstd compressed)
    println!("Loading libraries from: {}", libs_dir.display());

    let libc = decompress_zstd(&std::fs::read(libs_dir.join("libc.so.zst"))?);
    let libcxx = decompress_zstd(&std::fs::read(libs_dir.join("libc++.so.zst"))?);
    let libcxxabi = decompress_zstd(&std::fs::read(libs_dir.join("libc++abi.so.zst"))?);
    let libpython = decompress_zstd(&std::fs::read(libs_dir.join("libpython3.14.so.zst"))?);
    let wasi_clocks = decompress_zstd(&std::fs::read(
        libs_dir.join("libwasi-emulated-process-clocks.so.zst"),
    )?);
    let wasi_signal =
        decompress_zstd(&std::fs::read(libs_dir.join("libwasi-emulated-signal.so.zst"))?);
    let wasi_mman =
        decompress_zstd(&std::fs::read(libs_dir.join("libwasi-emulated-mman.so.zst"))?);
    let wasi_getpid =
        decompress_zstd(&std::fs::read(libs_dir.join("libwasi-emulated-getpid.so.zst"))?);
    let adapter = decompress_zstd(&std::fs::read(
        libs_dir.join("wasi_snapshot_preview1.reactor.wasm.zst"),
    )?);

    println!("  libc.so: {} bytes", libc.len());
    println!("  libc++.so: {} bytes", libcxx.len());
    println!("  libc++abi.so: {} bytes", libcxxabi.len());
    println!("  libpython3.14.so: {} bytes", libpython.len());
    println!(
        "  libwasi-emulated-process-clocks.so: {} bytes",
        wasi_clocks.len()
    );
    println!("  libwasi-emulated-signal.so: {} bytes", wasi_signal.len());
    println!("  libwasi-emulated-mman.so: {} bytes", wasi_mman.len());
    println!("  libwasi-emulated-getpid.so: {} bytes", wasi_getpid.len());

    // Parse the runtime.wit file
    let wit_path = project_root.join("crates/eryx-runtime/runtime.wit");
    println!("Loading WIT from: {}", wit_path.display());

    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve.push_path(&wit_path)?;
    let world_id = resolve.select_world(&[pkg_id], Some("sandbox"))?;

    println!("Parsed WIT world: sandbox");

    // Generate bindings pointing to our runtime
    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some("liberyx_runtime.so".to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
    };

    println!("Generating wit-dylib bindings...");
    let mut bindings = wit_dylib::create(&resolve, world_id, Some(&mut opts));

    // Embed component metadata
    embed_component_metadata(&mut bindings, &resolve, world_id, StringEncoding::UTF8)?;
    println!("Bindings size: {} bytes", bindings.len());

    // Link all libraries together
    // Order matters! Dependencies must come before dependents (same as build.rs)
    println!("Linking all libraries into component...");

    let linker = Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        // WASI emulation libs
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)?
        .library("libwasi-emulated-signal.so", &wasi_signal, false)?
        .library("libwasi-emulated-mman.so", &wasi_mman, false)?
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)?
        // C/C++ runtime
        .library("libc.so", &libc, false)?
        .library("libc++abi.so", &libcxxabi, false)?
        .library("libc++.so", &libcxx, false)?
        // Python
        .library("libpython3.14.so", &libpython, false)?
        // Our runtime and bindings
        .library("liberyx_runtime.so", &runtime, false)?
        .library("liberyx_bindings.so", &bindings, false)?
        // WASI adapter
        .adapter("wasi_snapshot_preview1", &adapter)?;

    let component = linker.encode()?;

    println!("SUCCESS! Linked component size: {} bytes", component.len());

    // Write the component to a file for inspection
    let output_path = manifest_dir.join("target/test_component.wasm");
    std::fs::write(&output_path, &component)?;
    println!("Wrote component to: {}", output_path.display());

    // Use wasm-tools to verify if available
    println!(
        "Run 'wasm-tools validate {}' to verify",
        output_path.display()
    );

    Ok(())
}
