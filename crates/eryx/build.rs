//! Build script for the eryx crate.
//!
//! When the `embedded` feature is enabled, this embeds the pre-compiled
//! runtime of every enabled guest language (the `python`, `javascript` and
//! `shell` features) into the binary for fast sandbox creation, and with the
//! `coreutils` feature, the shell's default commands (`coreutils.cwasm`,
//! located the same way through `ERYX_COREUTILS_CWASM`, the cache as
//! `coreutils-v<version>-<arch>-<os>.cwasm`, then `../eryx-runtime/`). With
//! `env-pack`, it embeds the JavaScript bundler (`eryx-bundler.cwasm`) the
//! same way: `ERYX_BUNDLER_CWASM`, the cache (`eryx-precompile setup
//! --bundler`), then the workspace, or, if none has it, an empty placeholder
//! (with a warning): eryx then loads the bundler from that cache at run time.
//!
//! Each `runtime-<language>.cwasm` file is located by checking (in order):
//! 1. An env var with an explicit path, embedded as is: `ERYX_RUNTIME_CWASM`
//!    for Python, `ERYX_RUNTIME_CWASM_JAVASCRIPT` and `ERYX_RUNTIME_CWASM_SHELL`
//! 2. `~/.cache/eryx/runtime-<language>-v<version>-<arch>-<os>.cwasm`
//!    (populated by `eryx-precompile setup`), used only if it matches its
//!    `.sha256` checksum file and, on Unix, is private to the current user
//! 3. `../eryx-runtime/runtime-<language>.cwasm` (workspace dev workflow)

// Build scripts should panic on errors, so expect/unwrap are appropriate here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(any(feature = "embedded", feature = "env-pack"))]
#[path = "build/cached_runtime.rs"]
mod cached_runtime;

#[path = "build/guest_sources.rs"]
mod guest_sources;

fn main() {
    guest_sources::prepare();
    #[cfg(feature = "embedded-stdlib")]
    embedded_stdlib::prepare();
    #[cfg(any(feature = "embedded", feature = "env-pack"))]
    embedded_runtime::prepare();
}

/// First 16 hex characters of SHA-256, used to name the extracted copies of
/// embedded files. Hashing here instead of at startup saves every process a
/// hash of the file, which for the runtime takes ~1s in unoptimized builds.
#[cfg(any(feature = "embedded-stdlib", feature = "env-pack"))]
fn short_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(&Sha256::digest(bytes)[..8])
}

#[cfg(feature = "embedded-stdlib")]
mod embedded_stdlib {
    pub fn prepare() {
        let path = "python-stdlib.tar.zst";
        println!("cargo::rerun-if-changed={path}");
        let bytes = std::fs::read(path).expect("Failed to read python-stdlib.tar.zst");
        println!(
            "cargo::rustc-env=ERYX_EMBEDDED_STDLIB_HASH={}",
            crate::short_sha256(&bytes)
        );
    }
}

#[cfg(any(feature = "embedded", feature = "env-pack"))]
mod embedded_runtime {
    use std::path::PathBuf;

    use crate::cached_runtime;

    /// A guest runtime (or embedded command) this build may embed.
    struct Runtime {
        /// What it is, for messages: the language, or `coreutils`.
        language: &'static str,
        /// Its file name without `.cwasm`, e.g. `runtime-python`.
        stem: &'static str,
        /// The `eryx-precompile setup` arguments that download and cache it.
        setup_args: &'static str,
        /// The workspace mise task that builds it.
        mise_task: &'static str,
        /// The cargo feature that embeds it.
        feature: &'static str,
        /// The env var naming an explicit `.cwasm` to embed.
        path_var: &'static str,
        /// Only embedded with the `embedded` feature too; the bundler is
        /// embedded by `env-pack` alone.
        with_embedded: bool,
        /// The rustc env var the build script sets to its short hash.
        hash_var: &'static str,
    }

    const RUNTIMES: [Runtime; 5] = [
        Runtime {
            language: "python",
            stem: "runtime-python",
            setup_args: "--language python",
            mise_task: "precompile-runtime-python",
            feature: "PYTHON",
            path_var: "ERYX_RUNTIME_CWASM",
            with_embedded: true,
            hash_var: "ERYX_EMBEDDED_RUNTIME_HASH",
        },
        Runtime {
            language: "javascript",
            stem: "runtime-javascript",
            setup_args: "--language javascript",
            mise_task: "precompile-runtime-javascript",
            feature: "JAVASCRIPT",
            path_var: "ERYX_RUNTIME_CWASM_JAVASCRIPT",
            with_embedded: true,
            hash_var: "ERYX_EMBEDDED_RUNTIME_HASH_JAVASCRIPT",
        },
        Runtime {
            language: "shell",
            stem: "runtime-shell",
            setup_args: "--language shell",
            mise_task: "precompile-runtime-shell",
            feature: "SHELL",
            path_var: "ERYX_RUNTIME_CWASM_SHELL",
            with_embedded: true,
            hash_var: "ERYX_EMBEDDED_RUNTIME_HASH_SHELL",
        },
        // The shell's default external commands (uutils coreutils), which
        // `setup --language shell` fetches along with the shell runtime.
        Runtime {
            language: "coreutils",
            stem: "coreutils",
            setup_args: "--language shell",
            mise_task: "precompile-coreutils",
            feature: "COREUTILS",
            path_var: "ERYX_COREUTILS_CWASM",
            with_embedded: true,
            hash_var: "ERYX_EMBEDDED_COREUTILS_HASH",
        },
        // The JavaScript bundler environment builds run (crates/eryx-bundler),
        // built by `mise run precompile-bundler`.
        Runtime {
            language: "bundler",
            stem: "eryx-bundler",
            setup_args: "--bundler",
            mise_task: "precompile-bundler",
            feature: "ENV_PACK",
            path_var: "ERYX_BUNDLER_CWASM",
            with_embedded: false,
            hash_var: "ERYX_EMBEDDED_BUNDLER_HASH",
        },
    ];

    impl Runtime {
        fn file_name(&self) -> String {
            format!("{}.cwasm", self.stem)
        }

        /// Write the runtime to embed and export its [`crate::short_sha256`].
        fn write(&self, out_dir: &std::path::Path, bytes: &[u8]) {
            std::fs::write(out_dir.join(self.file_name()), bytes)
                .unwrap_or_else(|e| panic!("Failed to write {}: {e}", self.file_name()));
            println!(
                "cargo::rustc-env={}={}",
                self.hash_var,
                crate::short_sha256(bytes)
            );
        }

        /// Find the runtime in the user's cache directory.
        ///
        /// Checks `$XDG_CACHE_HOME/eryx/` or `$HOME/.cache/eryx/` for
        /// `runtime-<language>-v<version>-<arch>-<os>.cwasm` (as written by
        /// `eryx-precompile setup`) for the target being built. A cached file
        /// that fails [`cached_runtime::read_verified`] is skipped with a
        /// warning.
        fn find_cached(&self) -> Option<(PathBuf, Vec<u8>)> {
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
            let path = cache_dir.join(cached_runtime::file_name(
                self.stem,
                &version,
                &format!("{arch}-{os}"),
            ));
            if std::fs::symlink_metadata(&path).is_err() {
                return None;
            }
            // Rebuild once `setup` repairs or replaces the cached runtime. A
            // missing path would count as always changed, so the checksum file
            // is only watched if it exists; `setup` rewrites the runtime
            // whenever it adds one.
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
                        "cargo::warning=Not using the cached {} runtime: {reason}. \
                         Run `eryx-precompile setup {}` again to rebuild it.",
                        self.language, self.setup_args
                    );
                    None
                }
            }
        }

        fn prepare(&self, out_dir: &std::path::Path) {
            let workspace_path = format!("../eryx-runtime/{}", self.file_name());
            println!("cargo::rerun-if-env-changed={}", self.path_var);
            println!("cargo::rerun-if-changed={workspace_path}");

            // docs.rs sandbox: no WASM artifacts available, write empty placeholder
            if std::env::var("DOCS_RS").is_ok() {
                self.write(out_dir, b"");
                return;
            }

            // 1. Explicit env var. A path that doesn't exist is a mistake, not
            // a reason to quietly embed some other runtime. The file is the
            // user's explicit choice, so it is embedded without the cache
            // checks below.
            let cwasm_path = std::env::var(self.path_var)
                .ok()
                .filter(|path| !path.is_empty())
                .map(|path| {
                    let path = PathBuf::from(path);
                    assert!(
                        path.is_file(),
                        "{} is set to {}, which is not a file",
                        self.path_var,
                        path.display()
                    );
                    path
                });

            let read = |path: PathBuf| {
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                (path, bytes)
            };
            let cwasm = cwasm_path.map(read);

            // 2. Cache directory (~/.cache/eryx/)
            let cwasm = cwasm.or_else(|| self.find_cached());

            // 3. Workspace dev path
            let cwasm = cwasm.or_else(|| {
                Some(PathBuf::from(&workspace_path))
                    .filter(|p| p.exists())
                    .map(read)
            });

            match cwasm {
                Some((path, bytes)) => {
                    println!("cargo::rerun-if-changed={}", path.display());
                    self.write(out_dir, &bytes);
                }
                // Precompile bootstrap: the precompile example builds without
                // any runtime, so it embeds an empty placeholder.
                None if std::env::var("ERYX_PRECOMPILE_BOOTSTRAP").is_ok() => {
                    self.write(out_dir, b"");
                }
                // The bundler can come from `setup --bundler`'s cache at run
                // time instead (eryx::cache::setup_bundler), so a build
                // without it (e.g. eryx-precompile's releases) still works.
                None if !self.with_embedded => {
                    println!(
                        "cargo::warning=Not embedding the {} ({}): it isn't built. \
                         Environment builds will load it from `eryx-precompile setup {}`'s \
                         cache instead.",
                        self.language,
                        self.file_name(),
                        self.setup_args
                    );
                    self.write(out_dir, b"");
                }
                None => {
                    let setup = format!(
                        "Use eryx-precompile (recommended for crates.io users):\n\n\
                         $ cargo binstall eryx-precompile\n\
                         $ eryx-precompile setup {}\n\n\
                         Or ",
                        self.setup_args
                    );
                    panic!(
                        "\n\
                        Pre-compiled {language} ({file}) not found.\n\
                        \n\
                        The `{feature}` feature needs it pre-compiled for your platform.\n\
                        \n\
                        {setup}set an explicit path:\n\
                        \n\
                        $ export {var}=/path/to/{file}\n\
                        \n\
                        Or build it from the workspace (for eryx developers):\n\
                        \n\
                        $ mise run {mise_task}\n\
                        ",
                        language = self.language,
                        feature = self.feature.to_lowercase().replace('_', "-"),
                        mise_task = self.mise_task,
                        file = self.file_name(),
                        var = self.path_var,
                    )
                }
            }
        }
    }

    /// Embed the runtime of every enabled language.
    pub fn prepare() {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));

        println!("cargo::rerun-if-env-changed=XDG_CACHE_HOME");
        println!("cargo::rerun-if-env-changed=HOME");
        println!("cargo::rerun-if-env-changed=DOCS_RS");

        let embedded = std::env::var_os("CARGO_FEATURE_EMBEDDED").is_some();
        for runtime in &RUNTIMES {
            if std::env::var_os(format!("CARGO_FEATURE_{}", runtime.feature)).is_some()
                && (embedded || !runtime.with_embedded)
            {
                runtime.prepare(&out_dir);
            }
        }
    }
}
