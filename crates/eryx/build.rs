//! Build script for the eryx crate.
//!
//! When the `embedded` feature is enabled, this embeds the pre-compiled Python
//! runtime into the binary for fast sandbox creation.
//!
//! The `runtime.cwasm` file is located by checking (in order):
//! 1. `ERYX_RUNTIME_CWASM` env var (explicit path, embedded as is)
//! 2. `~/.cache/eryx/runtime-v<version>-<arch>-<os>.cwasm` (populated by
//!    `eryx-precompile setup`), used only if it matches its `.sha256` checksum
//!    file and, on Unix, is private to the current user
//! 3. `../eryx-runtime/runtime.cwasm` (workspace dev workflow)

// Build scripts should panic on errors, so expect/unwrap are appropriate here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(feature = "embedded")]
#[path = "build/cached_runtime.rs"]
mod cached_runtime;

fn main() {
    #[cfg(feature = "embedded")]
    embedded_runtime::prepare();
}

#[cfg(feature = "embedded")]
mod embedded_runtime {
    use std::path::PathBuf;

    use crate::cached_runtime;

    /// Check if we're building the precompile example (which bootstraps without runtime.cwasm).
    fn is_precompile_bootstrap() -> bool {
        // The precompile example needs to build without runtime.cwasm existing.
        // Check if ERYX_PRECOMPILE_BOOTSTRAP is set to skip the file check.
        std::env::var("ERYX_PRECOMPILE_BOOTSTRAP").is_ok()
    }

    /// Find runtime.cwasm in the user's cache directory.
    ///
    /// Checks `$XDG_CACHE_HOME/eryx/` or `$HOME/.cache/eryx/` for
    /// `runtime-v<version>-<arch>-<os>.cwasm` (as written by `eryx-precompile
    /// setup`) for the target being built. A cached file that fails
    /// [`cached_runtime::read_verified`] is skipped with a warning.
    fn find_cached_runtime() -> Option<(PathBuf, Vec<u8>)> {
        let version = std::env::var("CARGO_PKG_VERSION").ok()?;
        let cache_dir = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg).join("eryx")
        } else {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()?;
            PathBuf::from(home).join(".cache").join("eryx")
        };

        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").ok()?;
        let os = std::env::var("CARGO_CFG_TARGET_OS").ok()?;
        let path = cache_dir.join(format!("runtime-v{version}-{arch}-{os}.cwasm"));
        if std::fs::symlink_metadata(&path).is_err() {
            return None;
        }
        // Rebuild once `setup` repairs or replaces the cached runtime. A
        // missing path would count as always changed, so the checksum file is
        // only watched if it exists; `setup` rewrites the runtime whenever it
        // adds one.
        println!("cargo::rerun-if-changed={}", path.display());
        let checksum = cached_runtime::checksum_path(&path);
        if std::fs::symlink_metadata(&checksum).is_ok() {
            println!("cargo::rerun-if-changed={}", checksum.display());
        }
        match cached_runtime::read_verified(&path) {
            Ok(bytes) => {
                eprintln!("Found cached runtime: {}", path.display());
                Some((path, bytes))
            }
            Err(reason) => {
                println!(
                    "cargo::warning=Not using the cached runtime: {reason}. \
                     Run `eryx-precompile setup` again to rebuild it."
                );
                None
            }
        }
    }

    pub fn prepare() {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));

        // Rebuild when env var or source files change
        println!("cargo::rerun-if-env-changed=ERYX_RUNTIME_CWASM");
        println!("cargo::rerun-if-env-changed=XDG_CACHE_HOME");
        println!("cargo::rerun-if-env-changed=HOME");
        println!("cargo::rerun-if-env-changed=DOCS_RS");
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.cwasm");

        // docs.rs sandbox: no WASM artifacts available, write empty placeholder
        if std::env::var("DOCS_RS").is_ok() {
            let dest = out_dir.join("runtime.cwasm");
            std::fs::write(&dest, b"").expect("Failed to write docs.rs placeholder runtime.cwasm");
            return;
        }

        // 1. Explicit env var. A path that doesn't exist is a mistake, not a
        // reason to quietly embed some other runtime. The file is the user's
        // explicit choice, so it is embedded without the cache checks below.
        let cwasm_path = std::env::var("ERYX_RUNTIME_CWASM")
            .ok()
            .filter(|path| !path.is_empty())
            .map(|path| {
                let path = PathBuf::from(path);
                assert!(
                    path.is_file(),
                    "ERYX_RUNTIME_CWASM is set to {}, which is not a file",
                    path.display()
                );
                path
            });

        let read = |path: PathBuf| {
            let bytes = std::fs::read(&path).expect("Failed to read runtime.cwasm");
            (path, bytes)
        };
        let cwasm = cwasm_path.map(read);

        // 2. Cache directory (~/.cache/eryx/)
        let cwasm = cwasm.or_else(find_cached_runtime);

        // 3. Workspace dev path
        let cwasm = cwasm.or_else(|| {
            Some(PathBuf::from("../eryx-runtime/runtime.cwasm"))
                .filter(|p| p.exists())
                .map(read)
        });

        match cwasm {
            Some((path, bytes)) => {
                println!("cargo::rerun-if-changed={}", path.display());
                let dest = out_dir.join("runtime.cwasm");
                std::fs::write(&dest, bytes).expect("Failed to write runtime.cwasm");
            }
            None => {
                if is_precompile_bootstrap() {
                    // Precompile bootstrap: create empty placeholder
                    let dest = out_dir.join("runtime.cwasm");
                    std::fs::write(&dest, b"").expect("Failed to write placeholder runtime.cwasm");
                    return;
                }
                panic!(
                    "\n\
                    Pre-compiled runtime (runtime.cwasm) not found.\n\
                    \n\
                    The `embedded` feature requires a pre-compiled WASM runtime for your platform.\n\
                    \n\
                    Option 1 — Use eryx-precompile (recommended for crates.io users):\n\
                    \n\
                    $ cargo binstall eryx-precompile\n\
                    $ eryx-precompile setup\n\
                    \n\
                    Option 2 — Set an explicit path:\n\
                    \n\
                    $ export ERYX_RUNTIME_CWASM=/path/to/runtime.cwasm\n\
                    \n\
                    Option 3 — Build from the workspace (for eryx developers):\n\
                    \n\
                    $ mise run precompile-eryx-runtime\n\
                    "
                );
            }
        }
    }
}
