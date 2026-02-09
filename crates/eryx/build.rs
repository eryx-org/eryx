//! Build script for the eryx crate.
//!
//! When the `embedded` feature is enabled, this embeds the pre-compiled Python
//! runtime into the binary for fast sandbox creation.
//!
//! The `runtime.cwasm` file is located by checking (in order):
//! 1. `ERYX_RUNTIME_CWASM` env var (explicit path)
//! 2. `~/.cache/eryx/runtime-v<version>*.cwasm` (populated by `eryx-precompile setup`)
//! 3. `../eryx-runtime/runtime.cwasm` (workspace dev workflow)

// Build scripts should panic on errors, so expect/unwrap are appropriate here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    #[cfg(feature = "embedded")]
    embedded_runtime::prepare();
}

#[cfg(feature = "embedded")]
mod embedded_runtime {
    use std::path::PathBuf;

    /// Check if we're building the precompile example (which bootstraps without runtime.cwasm).
    fn is_precompile_bootstrap() -> bool {
        // The precompile example needs to build without runtime.cwasm existing.
        // Check if ERYX_PRECOMPILE_BOOTSTRAP is set to skip the file check.
        std::env::var("ERYX_PRECOMPILE_BOOTSTRAP").is_ok()
    }

    /// Find runtime.cwasm in the user's cache directory.
    ///
    /// Checks `$XDG_CACHE_HOME/eryx/` or `$HOME/.cache/eryx/` for files matching
    /// `runtime-v<version>*.cwasm` (as written by `eryx-precompile setup`).
    fn find_cached_runtime() -> Option<PathBuf> {
        let version = std::env::var("CARGO_PKG_VERSION").ok()?;
        let cache_dir = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg).join("eryx")
        } else {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()?;
            PathBuf::from(home).join(".cache").join("eryx")
        };

        let prefix = format!("runtime-v{version}");
        let entries = std::fs::read_dir(&cache_dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(&prefix) && name_str.ends_with(".cwasm") {
                let path = entry.path();
                eprintln!("Found cached runtime: {}", path.display());
                return Some(path);
            }
        }
        None
    }

    pub fn prepare() {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));

        // Rebuild when env var or source files change
        println!("cargo::rerun-if-env-changed=ERYX_RUNTIME_CWASM");
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.cwasm");

        // 1. Explicit env var
        let cwasm_path = std::env::var("ERYX_RUNTIME_CWASM")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.exists());

        // 2. Cache directory (~/.cache/eryx/)
        let cwasm_path = cwasm_path.or_else(find_cached_runtime);

        // 3. Workspace dev path
        let cwasm_path = cwasm_path.or_else(|| {
            Some(PathBuf::from("../eryx-runtime/runtime.cwasm")).filter(|p| p.exists())
        });

        match cwasm_path {
            Some(path) => {
                let dest = out_dir.join("runtime.cwasm");
                std::fs::copy(&path, &dest).expect("Failed to copy runtime.cwasm");
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
