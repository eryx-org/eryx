// Build scripts should panic on failure, so expect/unwrap are appropriate
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Build script for eryx-runtime.
//!
//! This builds the guest runtimes whose interpreter is a wit-dylib shared
//! library: CPython (`eryx-guest-python`, on `BUILD_ERYX_RUNTIME`) and
//! QuickJS (`eryx-guest-js`, on `BUILD_ERYX_JS_RUNTIME`). For each:
//!
//! 1. Compile the guest crate as a PIC staticlib for wasm32-wasip1
//! 2. Compile clock_stubs.c with WASI SDK clang
//! 3. Link both into the interpreter shared library
//! 4. Generate wit-dylib bindings for the `sandbox` world
//! 5. Link everything into `runtime-<language>.wasm`
//!
//! For Python it also writes the late-linking artifacts (the interpreter and
//! bindings, zstd-compressed) used by preinit and native extensions. The
//! shell guest is a plain wasip2 component and isn't built here.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use wit_component::{StringEncoding, embed_component_metadata};

#[path = "src/link_order.rs"]
mod link_order;

/// Everything the late-linking artifacts are built from, relative to the
/// workspace root. These only exist in a git checkout, not in the crates.io
/// package.
const GUEST_SOURCES: [&str; 8] = [
    "crates/eryx-guest-python/src",
    "crates/eryx-guest-python/clock_stubs.c",
    "crates/eryx-guest-python/Cargo.toml",
    "crates/eryx-guest-call/src",
    "crates/eryx-guest-call/Cargo.toml",
    "crates/wit-dylib-ffi/src",
    "crates/wit-dylib-ffi/Cargo.toml",
    "crates/eryx-runtime/wit",
];

/// Records which guest sources a set of late-linking artifacts was built from,
/// next to the artifacts in `prebuilt/` and in OUT_DIR. See [`guest_sources_hash`].
const SOURCES_STAMP: &str = "guest-sources.hash";

/// What the JS guest is built from, relative to the workspace root.
const JS_GUEST_SOURCES: [&str; 5] = [
    "crates/eryx-guest-js",
    "crates/eryx-guest-call",
    "crates/eryx-guest-python/clock_stubs.c",
    "crates/wit-dylib-ffi/src",
    "crates/eryx-runtime/wit",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let wasm_runtime_dir = workspace_root.join("crates/eryx-guest-python");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));

    let sources_hash = guest_sources_hash(workspace_root);
    if sources_hash.is_some() {
        // Rerun if anything the guest is built from changes: its sources, its
        // dependencies, the workspace profiles, and the toolchain pins.
        for source in GUEST_SOURCES
            .iter()
            .chain(&["Cargo.lock", "Cargo.toml", "mise.toml"])
        {
            println!(
                "cargo::rerun-if-changed={}",
                workspace_root.join(source).display()
            );
        }
        println!("cargo::rerun-if-env-changed=WASI_SDK_PATH");
    }
    // Rerun if the build flag changes
    println!("cargo::rerun-if-env-changed=BUILD_ERYX_RUNTIME");
    println!("cargo::rerun-if-env-changed=ERYX_REQUIRE_FRESH_PREBUILT");
    // Rerun if pre-built artifacts change
    println!(
        "cargo::rerun-if-changed={}",
        manifest_dir.join("prebuilt").display()
    );
    // Rerun if the vendored CPython/libc blobs are re-vendored. Without this a
    // warm cargo cache keeps the previously linked runtime-python.wasm, so a lib swap
    // silently tests the old component.
    println!(
        "cargo::rerun-if-changed={}",
        manifest_dir.join("libs").display()
    );
    // Rerun if docs.rs env changes
    println!("cargo::rerun-if-env-changed=DOCS_RS");
    // The JS guest is only built on request, so only watched then: changing it
    // must not re-run the Python build.
    println!("cargo::rerun-if-env-changed=BUILD_ERYX_JS_RUNTIME");
    let build_js = env::var("BUILD_ERYX_JS_RUNTIME").is_ok();
    if build_js {
        for source in JS_GUEST_SOURCES {
            println!(
                "cargo::rerun-if-changed={}",
                workspace_root.join(source).display()
            );
        }
    }

    // docs.rs sandbox: no WASM build tools available, write empty placeholders
    if env::var("DOCS_RS").is_ok() {
        if env::var("CARGO_FEATURE_PREINIT").is_ok() {
            std::fs::write(out_dir.join("liberyx_runtime.so.zst"), b"")
                .expect("failed to write docs.rs placeholder runtime");
            std::fs::write(out_dir.join("liberyx_bindings.so.zst"), b"")
                .expect("failed to write docs.rs placeholder bindings");
        }
        return;
    }

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

    let out_stamp = out_dir.join(SOURCES_STAMP);

    // A committed prebuilt/ is an input, not a build output, so a guest change
    // that forgot to refresh it would otherwise silently run the old guest
    // code. Warn by default: the check is only as good as the stamp, and a
    // stale snapshot still links. CI sets ERYX_REQUIRE_FRESH_PREBUILT to fail
    // instead, and to check even builds that don't use prebuilt/ themselves,
    // since its later jobs replace prebuilt/ with freshly built artifacts.
    let require_fresh = env::var("ERYX_REQUIRE_FRESH_PREBUILT").is_ok();
    let uses_prebuilt = !build_requested && preinit && has_prebuilt;
    if let Some(hash) = &sources_hash
        && (require_fresh || uses_prebuilt)
        && read_stamp(&prebuilt_dir.join(SOURCES_STAMP)).as_ref() != Some(hash)
    {
        let msg = "crates/eryx-runtime/prebuilt/ was not built from the current guest sources, \
                   so preinit builds will use stale guest code. \
                   Run `mise run update-prebuilt` to rebuild it.";
        if require_fresh {
            panic!("{msg} (failing because ERYX_REQUIRE_FRESH_PREBUILT is set)");
        }
        println!("cargo::warning={msg}");
    }

    if build_js {
        build_js_runtime(&manifest_dir, workspace_root);
    }

    if build_requested {
        // Full build requested - build everything from scratch
        build_late_linking(&manifest_dir, &wasm_runtime_dir, sources_hash.as_deref());
    } else if preinit {
        // preinit feature (and native-extensions which implies it) needs .so.zst files in OUT_DIR
        //
        // IMPORTANT: Always prefer prebuilt/ artifacts over cached OUT_DIR artifacts!
        // The Rust cache may contain stale OUT_DIR artifacts from a previous build
        // (e.g., from main branch) that don't match the current branch's WIT.
        // CI overwrites prebuilt/ with the output of the same run's
        // build-runtime-python job, so there it always matches the current branch.
        if has_prebuilt {
            eprintln!("Using pre-built late-linking artifacts from prebuilt/");
            std::fs::copy(&prebuilt_runtime, &out_runtime_zst)
                .expect("failed to copy prebuilt runtime");
            std::fs::copy(&prebuilt_bindings, &out_bindings_zst)
                .expect("failed to copy prebuilt bindings");
            // Keep OUT_DIR's stamp describing what is now in OUT_DIR.
            let prebuilt_stamp = read_stamp(&prebuilt_dir.join(SOURCES_STAMP));
            match &prebuilt_stamp {
                Some(stamp) => std::fs::write(&out_stamp, stamp),
                None => std::fs::remove_file(&out_stamp).or(Ok(())),
            }
            .expect("failed to update OUT_DIR sources stamp");
        } else if has_out_artifacts
            && (sources_hash.is_none() || read_stamp(&out_stamp) == sources_hash)
        {
            // No prebuilt available, use existing OUT_DIR artifacts (local dev)
            eprintln!("Using existing late-linking artifacts from OUT_DIR");
        } else {
            // No usable artifacts anywhere, need to build from scratch
            eprintln!("Building late-linking artifacts from scratch...");
            build_late_linking(&manifest_dir, &wasm_runtime_dir, sources_hash.as_deref());
        }
    }

    // Emit a content fingerprint for the late-linking artifacts so that cargo
    // recompiles eryx-runtime (and its dependents) when their content changes.
    // Without this, `include_bytes!` in linker.rs bakes stale bytes into the
    // cached .rlib even after build.rs re-copies fresh .so.zst files to OUT_DIR.
    if out_runtime_zst.exists() && out_bindings_zst.exists() {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        std::fs::read(&out_runtime_zst)
            .expect("failed to read runtime .so.zst for fingerprint")
            .hash(&mut hasher);
        std::fs::read(&out_bindings_zst)
            .expect("failed to read bindings .so.zst for fingerprint")
            .hash(&mut hasher);
        println!(
            "cargo::rustc-env=ERYX_LATE_LINKING_HASH={:016x}",
            hasher.finish()
        );
    }
}

/// Build the late-linking artifacts and runtime-python.wasm, and stamp OUT_DIR with
/// the hash of the guest sources they were built from.
fn build_late_linking(manifest_dir: &Path, wasm_runtime_dir: &Path, sources_hash: Option<&str>) {
    let runtime_so = build_python_runtime(wasm_runtime_dir);
    build_python_component(manifest_dir, &runtime_so);
    if let Some(hash) = sources_hash {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
        std::fs::write(out_dir.join(SOURCES_STAMP), hash).expect("failed to write sources stamp");
    }
}

/// Hash the files under [`GUEST_SOURCES`], which the late-linking artifacts
/// are built from.
///
/// Returns `None` outside a git checkout (the crates.io package has no guest
/// sources), where there is nothing to compare `prebuilt/` against.
///
/// The toolchain pins in mise.toml are deliberately not hashed: some CI jobs
/// replace mise.toml, and this runs on every preinit build, so it must never
/// depend on that file. A pin bump needs `mise run update-prebuilt` by hand.
///
/// Uses FNV-1a rather than `DefaultHasher`, whose output may change between
/// Rust releases: the hash is committed in `prebuilt/`, so it has to mean the
/// same thing on every toolchain and platform. Carriage returns are skipped so
/// a CRLF checkout hashes like an LF one.
fn guest_sources_hash(workspace_root: &Path) -> Option<String> {
    if !workspace_root.join(GUEST_SOURCES[0]).exists() {
        return None;
    }

    let mut files = Vec::new();
    for source in GUEST_SOURCES {
        collect_files(&workspace_root.join(source), &mut files);
    }
    files.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes.iter().filter(|&&b| b != b'\r') {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash ^= 0xff;
    };
    for file in &files {
        let relative = file.strip_prefix(workspace_root).unwrap();
        // Hash the path with `/` separators so Windows agrees with Unix.
        let components: Vec<_> = relative.iter().map(|c| c.to_string_lossy()).collect();
        feed(components.join("/").as_bytes());
        feed(&std::fs::read(file).expect("failed to read guest source"));
    }

    Some(format!("{hash:016x}"))
}

/// Push `path` if it is a file, or every file under it if it is a directory.
fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).expect("failed to read guest source dir") {
            collect_files(&entry.expect("failed to read dir entry").path(), files);
        }
    } else {
        files.push(path.to_path_buf());
    }
}

fn read_stamp(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Read a `[vars]` entry from the workspace `mise.toml`, the single source of
/// truth for the toolchain pins shared with the mise tasks and CI. Only called
/// when building the guest, which needs a full checkout anyway.
fn mise_var(workspace_root: &Path, key: &str) -> String {
    let mise = std::fs::read_to_string(workspace_root.join("mise.toml"))
        .expect("failed to read mise.toml");
    let prefix = format!("{key} = \"");
    mise.lines()
        .find_map(|line| line.strip_circumfix(prefix.as_str(), '"'))
        .unwrap_or_else(|| panic!("mise.toml has no `{key} = \"...\"` in [vars]"))
        .to_string()
}

/// Build the eryx-guest-python shared library, and its compressed copy for
/// late linking. Returns the path to the built liberyx_runtime.so.
fn build_python_runtime(wasm_runtime_dir: &Path) -> PathBuf {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let workspace_root = wasm_runtime_dir.parent().unwrap().parent().unwrap();
    let runtime_so = build_guest_library(
        workspace_root,
        &GuestLibrary {
            crate_dir: wasm_runtime_dir,
            staticlib: "liberyx_guest_python.a",
            library: "liberyx_runtime.so",
            target_dir: "wasm-runtime-target",
            build_c_deps: false,
        },
    );

    // Also copy to a stable location in the guest crate
    let stable_location = wasm_runtime_dir.join("target/liberyx_runtime.so");
    std::fs::create_dir_all(wasm_runtime_dir.join("target")).ok();
    std::fs::copy(&runtime_so, &stable_location).expect("failed to copy runtime.so");
    eprintln!("Copied to: {}", stable_location.display());

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

/// A guest crate built into a wit-dylib interpreter shared library.
struct GuestLibrary<'a> {
    /// The guest crate's directory.
    crate_dir: &'a Path,
    /// The staticlib cargo produces for it.
    staticlib: &'a str,
    /// The shared library to link it into.
    library: &'a str,
    /// The nested cargo target directory, under OUT_DIR.
    target_dir: &'a str,
    /// Whether the crate compiles C dependencies with the cc crate (QuickJS),
    /// which then need the WASI SDK and -fPIC.
    build_c_deps: bool,
}

/// Build a guest crate into its interpreter shared library, in OUT_DIR.
fn build_guest_library(workspace_root: &Path, guest: &GuestLibrary<'_>) -> PathBuf {
    // Use a separate target directory inside OUT_DIR to avoid cargo lock contention
    // This is the key insight from componentize-py - nested cargo must use different target
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let nested_target_dir = out_dir.join(guest.target_dir);

    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    let toolchain = mise_var(workspace_root, "guest_rust_toolchain");
    let wasi_sdk_version = mise_var(workspace_root, "wasi_sdk_version");

    // Find WASI SDK
    let wasi_sdk = find_wasi_sdk(&wasi_sdk_version).unwrap_or_else(|| {
        panic!(
            "WASI SDK not found. Set WASI_SDK_PATH, or install it with: \
             mise install github:WebAssembly/wasi-sdk@{wasi_sdk_version}"
        )
    });
    let clang = wasi_sdk.join("bin/clang");
    let sysroot = wasi_sdk.join("share/wasi-sysroot");

    eprintln!("Building {}...", guest.crate_dir.display());
    eprintln!("  WASI SDK: {}", wasi_sdk.display());

    // Build the staticlib with PIC for wasm32-wasip1, with the nightly pinned
    // in mise.toml (build-std needs nightly).
    let mut cmd = Command::new("rustup");
    cmd.current_dir(guest.crate_dir)
        .arg("run")
        .arg(&toolchain)
        .arg("cargo")
        .arg("build")
        .arg("--locked")
        .arg("-Z")
        .arg("build-std=panic_abort,std")
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--release");

    // Strip inherited Rust/Cargo env vars to prevent lock contention, but keep
    // the ones that locate the toolchains and registry, and offline mode.
    const KEEP_ENV: [&str; 3] = ["CARGO_HOME", "RUSTUP_HOME", "CARGO_NET_OFFLINE"];
    for (key, _) in env::vars_os() {
        if let Some(key_str) = key.to_str()
            && (key_str.starts_with("RUST") || key_str.starts_with("CARGO"))
            && !KEEP_ENV.contains(&key_str)
        {
            cmd.env_remove(&key);
        }
    }

    // Set the env vars we need
    // IMPORTANT: Use separate target dir to avoid cargo lock contention
    cmd.env("RUSTFLAGS", "-C relocation-model=pic");
    cmd.env("CARGO_TARGET_DIR", &nested_target_dir);
    if guest.build_c_deps {
        // rquickjs-sys compiles quickjs-ng with `$WASI_SDK/bin/clang` and would
        // otherwise download its own WASI SDK. It overwrites `CFLAGS`, but the
        // cc crate prefers the target-specific variable, which also lets us add
        // -fPIC so the C objects can go into a shared library.
        cmd.env("WASI_SDK", &wasi_sdk);
        cmd.env(
            "CFLAGS_wasm32_wasip1",
            format!("--sysroot={} -fPIC", sysroot.display()),
        );
    }

    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {e}", guest.staticlib));
    if !status.success() {
        panic!("cargo build for {} failed", guest.staticlib);
    }

    let staticlib = nested_target_dir
        .join("wasm32-wasip1/release")
        .join(guest.staticlib);
    if !staticlib.exists() {
        panic!("staticlib not found at {}", staticlib.display());
    }

    eprintln!("Compiling clock stubs...");

    // Compile clock_stubs.c
    let clock_stubs_o = out_dir.join(format!("{}-clock_stubs.o", guest.target_dir));
    let status = Command::new(&clang)
        .arg("--target=wasm32-wasip1")
        .arg(format!("--sysroot={}", sysroot.display()))
        .arg("-fPIC")
        .arg("-c")
        .arg(workspace_root.join("crates/eryx-guest-python/clock_stubs.c"))
        .arg("-o")
        .arg(&clock_stubs_o)
        .status()
        .expect("failed to run clang");
    if !status.success() {
        panic!("clang failed to compile clock_stubs.c");
    }

    eprintln!("Linking shared library...");

    // Link shared library
    let runtime_so = out_dir.join(guest.library);
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
        panic!("clang failed to link {}", guest.library);
    }

    eprintln!("Built: {}", runtime_so.display());
    runtime_so
}

/// Read a vendored library from `libs/`, decompressing it in memory.
fn read_lib(manifest_dir: &Path, file: &str) -> Vec<u8> {
    let path = manifest_dir.join("libs").join(format!("{file}.zst"));
    let compressed =
        std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    zstd::decode_all(compressed.as_slice())
        .unwrap_or_else(|e| panic!("failed to decompress {}: {e}", path.display()))
}

/// Generate the wit-dylib bindings for the `sandbox` world, with `interpreter`
/// as the shared library that implements it.
fn sandbox_bindings(manifest_dir: &Path, interpreter: &str) -> Vec<u8> {
    // Parse WIT directory (includes deps/)
    let wit_dir = manifest_dir.join("wit");
    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve
        .push_dir(&wit_dir)
        .expect("failed to parse WIT directory");
    // Select the sandbox world from the eryx:sandbox package
    let world_id = resolve
        .select_world(&[pkg_id], Some("sandbox"))
        .expect("failed to select world");

    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some(interpreter.to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
        stack_pointer: wit_dylib::StackPointer::Global,
    };

    let mut bindings =
        wit_dylib::create(&resolve, world_id, Some(&mut opts)).expect("failed to create bindings");
    embed_component_metadata(
        &mut bindings,
        &resolve,
        world_id,
        StringEncoding::UTF8,
        false,
    )
    .expect("failed to embed component metadata");
    bindings
}

/// Link `libs`, in link order, into a component with the WASI adapter, and
/// write it to `crates/eryx-runtime/<file>`.
fn encode_component(manifest_dir: &Path, libs: &[(&str, &[u8])], file: &str) {
    // wit-component 0.257 turned `Linker` into a `&mut self` builder and moved
    // `validate`/`adapter` onto the inner `ComponentEncoder`.
    let mut linker = wit_component::Linker::default();
    linker.use_built_in_libdl(true);
    linker.encoder().validate(true);
    for (name, bytes) in libs {
        linker
            .library(name, bytes, false)
            .unwrap_or_else(|e| panic!("failed to add {name}: {e}"));
    }

    // WASI adapter
    linker
        .encoder()
        .adapter(
            "wasi_snapshot_preview1",
            &read_lib(manifest_dir, "wasi_snapshot_preview1.reactor.wasm"),
        )
        .expect("failed to add WASI adapter");

    let component = linker.encode().expect("failed to encode component");

    let component_path = manifest_dir.join(file);
    std::fs::write(&component_path, &component)
        .unwrap_or_else(|e| panic!("failed to write {file}: {e}"));

    eprintln!(
        "Built component: {} ({} bytes)",
        component_path.display(),
        component.len()
    );
}

/// Build runtime-python.wasm, and the compressed bindings for late linking.
fn build_python_component(manifest_dir: &Path, runtime_so: &Path) {
    eprintln!("Building Python component...");

    let read = |file| read_lib(manifest_dir, file);
    let libc = read("libc.so");
    let libcxx = read("libc++.so");
    let libcxxabi = read("libc++abi.so");
    let wasi_clocks = read("libwasi-emulated-process-clocks.so");
    let wasi_signal = read("libwasi-emulated-signal.so");
    let wasi_mman = read("libwasi-emulated-mman.so");
    let wasi_getpid = read("libwasi-emulated-getpid.so");
    let libpython = read("libpython3.14.so");
    let runtime = std::fs::read(runtime_so).expect("failed to read liberyx_runtime.so");
    let bindings = sandbox_bindings(manifest_dir, "liberyx_runtime.so");

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

    let libs = link_order::BaseLibraries {
        wasi_clocks: &wasi_clocks,
        wasi_signal: &wasi_signal,
        wasi_mman: &wasi_mman,
        wasi_getpid: &wasi_getpid,
        libc: &libc,
        libcxxabi: &libcxxabi,
        libcxx: &libcxx,
        libpython: &libpython,
        runtime: &runtime,
        bindings: &bindings,
    };
    encode_component(manifest_dir, &libs.in_link_order(), "runtime-python.wasm");
}

/// Build the QuickJS guest into runtime-javascript.wasm: the same pipeline as
/// CPython, against the same vendored libc, minus libpython and libc++.
fn build_js_runtime(manifest_dir: &Path, workspace_root: &Path) {
    let runtime_so = build_guest_library(
        workspace_root,
        &GuestLibrary {
            crate_dir: &workspace_root.join("crates/eryx-guest-js"),
            staticlib: "liberyx_guest_js.a",
            library: "liberyx_js_runtime.so",
            target_dir: "wasm-js-runtime-target",
            build_c_deps: true,
        },
    );
    eprintln!("Building JavaScript component...");
    let read = |file| read_lib(manifest_dir, file);
    let wasi_clocks = read("libwasi-emulated-process-clocks.so");
    let wasi_signal = read("libwasi-emulated-signal.so");
    let wasi_mman = read("libwasi-emulated-mman.so");
    let wasi_getpid = read("libwasi-emulated-getpid.so");
    let libc = read("libc.so");
    let runtime = std::fs::read(&runtime_so).expect("failed to read liberyx_js_runtime.so");
    let bindings = sandbox_bindings(manifest_dir, "liberyx_js_runtime.so");
    let libs: [(&str, &[u8]); 7] = [
        ("libwasi-emulated-process-clocks.so", &wasi_clocks),
        ("libwasi-emulated-signal.so", &wasi_signal),
        ("libwasi-emulated-mman.so", &wasi_mman),
        ("libwasi-emulated-getpid.so", &wasi_getpid),
        ("libc.so", &libc),
        ("liberyx_js_runtime.so", &runtime),
        ("liberyx_bindings.so", &bindings),
    ];
    encode_component(manifest_dir, &libs, "runtime-javascript.wasm");
}

/// Find the WASI SDK: `WASI_SDK_PATH` if set, else the mise-managed install of
/// the version pinned in mise.toml.
fn find_wasi_sdk(version: &str) -> Option<PathBuf> {
    if let Ok(path) = env::var("WASI_SDK_PATH") {
        let path = PathBuf::from(path);
        if path.join("bin/clang").exists() {
            return Some(path);
        }
    }

    let output = Command::new("mise")
        .args(["where", &format!("github:WebAssembly/wasi-sdk@{version}")])
        .output()
        .ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (output.status.success() && path.join("bin/clang").exists()).then_some(path)
}
