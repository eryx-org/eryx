// Build scripts should panic on failure, so expect/unwrap are appropriate
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Build script for eryx-runtime.
//!
//! This builds:
//! 1. The eryx-wasm-runtime shared library (liberyx_runtime.so) that implements
//!    the wit-dylib interpreter interface for Python execution.
//! 2. The final WASM component (runtime.wasm) by linking all libraries together.
//!
//! The build process:
//! 1. Compile eryx-wasm-runtime staticlib with PIC for wasm32-wasip1
//! 2. Compile clock_stubs.c with WASI SDK clang
//! 3. Link into liberyx_runtime.so
//! 4. Generate wit-dylib bindings
//! 5. Link all libraries into final runtime.wasm component

use std::env;
use std::path::PathBuf;
use std::process::Command;

use wit_component::{StringEncoding, embed_component_metadata};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let wasm_runtime_dir = manifest_dir.parent().unwrap().join("eryx-wasm-runtime");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));

    // Rerun if eryx-wasm-runtime sources change
    println!(
        "cargo::rerun-if-changed={}",
        wasm_runtime_dir.join("src").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        wasm_runtime_dir.join("clock_stubs.c").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        wasm_runtime_dir.join("Cargo.toml").display()
    );
    // Rerun if WIT changes
    println!(
        "cargo::rerun-if-changed={}",
        manifest_dir.join("runtime.wit").display()
    );
    // Rerun if the build flag changes
    println!("cargo::rerun-if-env-changed=BUILD_ERYX_RUNTIME");
    // Rerun if pre-built artifacts change
    println!(
        "cargo::rerun-if-changed={}",
        manifest_dir.join("prebuilt").display()
    );

    // Only build when explicitly requested via BUILD_ERYX_RUNTIME env var
    // Previously we also built in release mode, but that causes issues in CI where
    // we pre-build the WASM and don't want clippy/doc to try rebuilding it
    let build_requested = env::var("BUILD_ERYX_RUNTIME").is_ok();
    let preinit = env::var("CARGO_FEATURE_PREINIT").is_ok();

    // Check for pre-built late-linking artifacts (for CI)
    let prebuilt_dir = manifest_dir.join("prebuilt");
    let prebuilt_runtime = prebuilt_dir.join("liberyx_runtime.so.zst");
    let prebuilt_bindings = prebuilt_dir.join("liberyx_bindings.so.zst");
    let has_prebuilt = prebuilt_runtime.exists() && prebuilt_bindings.exists();

    // Check if late-linking artifacts already exist in OUT_DIR (from previous build)
    let out_runtime_zst = out_dir.join("liberyx_runtime.so.zst");
    let out_bindings_zst = out_dir.join("liberyx_bindings.so.zst");
    let has_out_artifacts = out_runtime_zst.exists() && out_bindings_zst.exists();

    if build_requested {
        // Full build requested - build everything from scratch
        let runtime_so = build_wasm_runtime(&wasm_runtime_dir);
        build_component(&manifest_dir, &runtime_so);
    } else if preinit {
        // preinit feature (and native-extensions which implies it) needs .so.zst files in OUT_DIR
        if has_out_artifacts {
            // Already have artifacts from a previous build - nothing to do
            eprintln!("Using existing late-linking artifacts from OUT_DIR");
        } else if has_prebuilt {
            // Use pre-built artifacts from prebuilt/ directory (CI)
            eprintln!("Using pre-built late-linking artifacts from prebuilt/");
            std::fs::copy(&prebuilt_runtime, &out_runtime_zst)
                .expect("failed to copy prebuilt runtime");
            std::fs::copy(&prebuilt_bindings, &out_bindings_zst)
                .expect("failed to copy prebuilt bindings");
        } else {
            // No artifacts anywhere, need to build from scratch
            eprintln!("Building late-linking artifacts from scratch...");
            let runtime_so = build_wasm_runtime(&wasm_runtime_dir);
            build_component(&manifest_dir, &runtime_so);
        }
    }
}

/// Build the eryx-wasm-runtime shared library.
/// Returns the path to the built liberyx_runtime.so.
fn build_wasm_runtime(wasm_runtime_dir: &PathBuf) -> PathBuf {
    // Use a separate target directory inside OUT_DIR to avoid cargo lock contention
    // This is the key insight from componentize-py - nested cargo must use different target
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let nested_target_dir = out_dir.join("wasm-runtime-target");

    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    // Find WASI SDK
    let wasi_sdk = find_wasi_sdk().expect(
        "WASI SDK not found. Install with: mise install github:WebAssembly/wasi-sdk@wasi-sdk-27",
    );
    let clang = wasi_sdk.join("bin/clang");
    let sysroot = wasi_sdk.join("share/wasi-sysroot");

    eprintln!("Building eryx-wasm-runtime...");
    eprintln!("  WASI SDK: {}", wasi_sdk.display());

    // Build eryx-wasm-runtime staticlib with PIC for wasm32-wasip1
    // Strip RUST*/CARGO* env vars to avoid cargo lock contention
    let mut cmd = Command::new("rustup");
    cmd.current_dir(wasm_runtime_dir)
        .arg("run")
        .arg("nightly")
        .arg("cargo")
        .arg("build")
        .arg("-Z")
        .arg("build-std=panic_abort,std")
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--release");

    // Strip inherited Rust/Cargo env vars to prevent lock contention
    for (key, _) in env::vars_os() {
        if let Some(key_str) = key.to_str()
            && (key_str.starts_with("RUST") || key_str.starts_with("CARGO"))
        {
            cmd.env_remove(&key);
        }
    }

    // Set the env vars we need
    // IMPORTANT: Use separate target dir to avoid cargo lock contention
    cmd.env("RUSTFLAGS", "-C relocation-model=pic");
    cmd.env("CARGO_TARGET_DIR", &nested_target_dir);

    let status = cmd
        .status()
        .expect("failed to run cargo build for eryx-wasm-runtime");
    if !status.success() {
        panic!("cargo build for eryx-wasm-runtime failed");
    }

    let staticlib = nested_target_dir.join("wasm32-wasip1/release/liberyx_wasm_runtime.a");
    if !staticlib.exists() {
        panic!("staticlib not found at {}", staticlib.display());
    }

    eprintln!("Compiling clock stubs...");

    // Compile clock_stubs.c
    let clock_stubs_o = out_dir.join("clock_stubs.o");
    let status = Command::new(&clang)
        .arg("--target=wasm32-wasip1")
        .arg(format!("--sysroot={}", sysroot.display()))
        .arg("-fPIC")
        .arg("-c")
        .arg(wasm_runtime_dir.join("clock_stubs.c"))
        .arg("-o")
        .arg(&clock_stubs_o)
        .status()
        .expect("failed to run clang");
    if !status.success() {
        panic!("clang failed to compile clock_stubs.c");
    }

    eprintln!("Linking shared library...");

    // Link shared library
    let runtime_so = out_dir.join("liberyx_runtime.so");
    let status = Command::new(&clang)
        .arg("--target=wasm32-wasip1")
        .arg(format!("--sysroot={}", sysroot.display()))
        .arg("-shared")
        .arg("-Wl,--allow-undefined")
        .arg("-o")
        .arg(&runtime_so)
        .arg("-Wl,--whole-archive")
        .arg(&staticlib)
        .arg("-Wl,--no-whole-archive")
        .arg(&clock_stubs_o)
        .status()
        .expect("failed to link shared library");
    if !status.success() {
        panic!("clang failed to link shared library");
    }

    // Also copy to a stable location in the wasm-runtime crate
    let stable_location = wasm_runtime_dir.join("target/liberyx_runtime.so");
    std::fs::create_dir_all(wasm_runtime_dir.join("target")).ok();
    std::fs::copy(&runtime_so, &stable_location).expect("failed to copy runtime.so");

    eprintln!("Built: {}", runtime_so.display());
    eprintln!("Copied to: {}", stable_location.display());

    // Export the path for downstream use
    println!("cargo::rustc-env=ERYX_RUNTIME_SO={}", runtime_so.display());

    // Also compress for late-linking support
    let runtime_bytes = std::fs::read(&runtime_so).expect("failed to read runtime.so");
    let compressed = zstd::encode_all(runtime_bytes.as_slice(), 19).expect("compress failed");
    let compressed_path = out_dir.join("liberyx_runtime.so.zst");
    std::fs::write(&compressed_path, &compressed).expect("failed to write compressed runtime");
    eprintln!(
        "Compressed runtime: {} bytes -> {} bytes",
        runtime_bytes.len(),
        compressed.len()
    );

    runtime_so
}

/// Decompress .zst files in libs/ to libs/decompressed/
fn decompress_libs(manifest_dir: &std::path::Path) {
    let libs_dir = manifest_dir.join("libs");
    let decompressed_dir = libs_dir.join("decompressed");

    std::fs::create_dir_all(&decompressed_dir).expect("failed to create decompressed dir");

    // List of files to decompress
    let files = [
        "libc.so",
        "libc++.so",
        "libc++abi.so",
        "libpython3.14.so",
        "libwasi-emulated-process-clocks.so",
        "libwasi-emulated-signal.so",
        "libwasi-emulated-mman.so",
        "libwasi-emulated-getpid.so",
        "wasi_snapshot_preview1.reactor.wasm",
    ];

    for file in &files {
        let compressed_path = libs_dir.join(format!("{}.zst", file));
        let decompressed_path = decompressed_dir.join(file);

        // Skip if already decompressed and newer than compressed
        if decompressed_path.exists() {
            let compressed_meta = std::fs::metadata(&compressed_path).ok();
            let decompressed_meta = std::fs::metadata(&decompressed_path).ok();

            if let (Some(c), Some(d)) = (compressed_meta, decompressed_meta)
                && let (Ok(c_time), Ok(d_time)) = (c.modified(), d.modified())
                && d_time >= c_time
            {
                continue;
            }
        }

        eprintln!("Decompressing {}...", file);
        let compressed = std::fs::read(&compressed_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", compressed_path.display(), e));
        let decompressed = zstd::decode_all(compressed.as_slice())
            .unwrap_or_else(|e| panic!("failed to decompress {}: {}", file, e));
        std::fs::write(&decompressed_path, &decompressed)
            .unwrap_or_else(|e| panic!("failed to write {}: {}", decompressed_path.display(), e));
    }
}

/// Build the WASM component by linking all libraries together.
fn build_component(manifest_dir: &std::path::Path, runtime_so: &std::path::Path) {
    eprintln!("Building WASM component...");

    // Ensure libs are decompressed
    decompress_libs(manifest_dir);

    let libs_dir = manifest_dir.join("libs/decompressed");

    // Load required shared libraries
    let libc = std::fs::read(libs_dir.join("libc.so")).expect("failed to read libc.so");
    let libcxx = std::fs::read(libs_dir.join("libc++.so")).expect("failed to read libc++.so");
    let libcxxabi =
        std::fs::read(libs_dir.join("libc++abi.so")).expect("failed to read libc++abi.so");
    let wasi_clocks = std::fs::read(libs_dir.join("libwasi-emulated-process-clocks.so"))
        .expect("failed to read libwasi-emulated-process-clocks.so");
    let wasi_signal = std::fs::read(libs_dir.join("libwasi-emulated-signal.so"))
        .expect("failed to read libwasi-emulated-signal.so");
    let wasi_mman = std::fs::read(libs_dir.join("libwasi-emulated-mman.so"))
        .expect("failed to read libwasi-emulated-mman.so");
    let wasi_getpid = std::fs::read(libs_dir.join("libwasi-emulated-getpid.so"))
        .expect("failed to read libwasi-emulated-getpid.so");
    let libpython =
        std::fs::read(libs_dir.join("libpython3.14.so")).expect("failed to read libpython3.14.so");
    let adapter = std::fs::read(libs_dir.join("wasi_snapshot_preview1.reactor.wasm"))
        .expect("failed to read wasi_snapshot_preview1.reactor.wasm");

    // Load our runtime
    let runtime = std::fs::read(runtime_so).expect("failed to read liberyx_runtime.so");

    // Parse the runtime.wit file
    let wit_path = manifest_dir.join("runtime.wit");
    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve.push_path(&wit_path).expect("failed to parse WIT");
    let world_id = resolve
        .select_world(&[pkg_id], Some("sandbox"))
        .expect("failed to select world");

    // Generate bindings pointing to our runtime
    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some("liberyx_runtime.so".to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
    };

    let mut bindings = wit_dylib::create(&resolve, world_id, Some(&mut opts));
    embed_component_metadata(&mut bindings, &resolve, world_id, StringEncoding::UTF8)
        .expect("failed to embed component metadata");

    // Also compress bindings for late-linking support
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let bindings_compressed = zstd::encode_all(bindings.as_slice(), 19).expect("compress failed");
    let bindings_path = out_dir.join("liberyx_bindings.so.zst");
    std::fs::write(&bindings_path, &bindings_compressed).expect("failed to write bindings");
    eprintln!(
        "Compressed bindings: {} bytes -> {} bytes",
        bindings.len(),
        bindings_compressed.len()
    );

    // Link all libraries together
    // Order matters! Dependencies must come before dependents
    let linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        // WASI emulation libs
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)
        .expect("failed to add wasi-clocks")
        .library("libwasi-emulated-signal.so", &wasi_signal, false)
        .expect("failed to add wasi-signal")
        .library("libwasi-emulated-mman.so", &wasi_mman, false)
        .expect("failed to add wasi-mman")
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)
        .expect("failed to add wasi-getpid")
        // C/C++ runtime
        .library("libc.so", &libc, false)
        .expect("failed to add libc")
        .library("libc++abi.so", &libcxxabi, false)
        .expect("failed to add libc++abi")
        .library("libc++.so", &libcxx, false)
        .expect("failed to add libc++")
        // Python
        .library("libpython3.14.so", &libpython, false)
        .expect("failed to add libpython")
        // Our runtime and bindings
        .library("liberyx_runtime.so", &runtime, false)
        .expect("failed to add eryx runtime")
        .library("liberyx_bindings.so", &bindings, false)
        .expect("failed to add bindings")
        // WASI adapter
        .adapter("wasi_snapshot_preview1", &adapter)
        .expect("failed to add WASI adapter");

    let component = linker.encode().expect("failed to encode component");

    // Write the component to runtime.wasm in the crate directory
    let component_path = manifest_dir.join("runtime.wasm");
    std::fs::write(&component_path, &component).expect("failed to write runtime.wasm");

    eprintln!(
        "Built component: {} ({} bytes)",
        component_path.display(),
        component.len()
    );

    // Export the path for downstream use
    println!(
        "cargo::rustc-env=ERYX_RUNTIME_WASM={}",
        component_path.display()
    );
}

/// Find WASI SDK in order of preference:
/// 1. WASI_SDK_PATH environment variable
/// 2. mise-managed installation
/// 3. Local project installation
fn find_wasi_sdk() -> Option<PathBuf> {
    // Check explicit env var
    if let Ok(path) = env::var("WASI_SDK_PATH") {
        let path = PathBuf::from(path);
        if path.join("bin/clang").exists() {
            return Some(path);
        }
    }

    // Try mise-managed WASI SDK
    if let Ok(output) = Command::new("mise")
        .args(["where", "github:WebAssembly/wasi-sdk"])
        .output()
        && output.status.success()
    {
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if path.join("bin/clang").exists() {
            return Some(path);
        }
    }

    // Try local project installation
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let workspace_root = manifest_dir.parent()?.parent()?;
    let local_path = workspace_root.join(".wasi-sdk/wasi-sdk-27.0-x86_64-linux");
    if local_path.join("bin/clang").exists() {
        return Some(local_path);
    }

    None
}
