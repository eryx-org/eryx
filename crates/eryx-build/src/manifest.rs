//! Per-CLI build manifest schema and loading.
//!
//! A manifest (`clis/<name>.toml`) declares everything the driver needs to turn
//! an upstream CLI's source into a WASI component: which language lane to use,
//! where the source lives, how to patch and build it, and where artifacts land.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// A fully-parsed CLI build manifest.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Command name (e.g. `gh`). Also the name the command is registered under.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// SPDX license expression of the upstream source.
    pub license: String,
    /// Build lane selecting the toolchain.
    pub lang: Lang,
    /// Upstream source location.
    pub source: Source,
    /// Build configuration.
    pub build: Build,
    /// Component-model settings (Go lane only; defaults for other lanes).
    #[serde(default)]
    pub component: Component,
    /// Optional cwasm pre-compilation.
    #[serde(default)]
    pub cwasm: Cwasm,
    /// External libraries built before the main project (C lane). Each exposes
    /// libs/headers the main build references via `{dep:NAME:src}` /
    /// `{dep:NAME:build}` placeholders in its `cflags`, `link_flags`,
    /// `cmake_flags` and `configure_flags`.
    #[serde(default)]
    pub deps: Vec<Dep>,
    /// Where built artifacts are written.
    pub output: Output,
}

/// A dependency library built before the main project (C lane only).
///
/// Built into `<dir>/build` with the wasi-sdk wasip2 toolchain; produces static
/// libs + headers the parent build links against. Any `shims` are compiled with
/// this dep's `cflags` + its include dir and linked into the parent — the hook
/// for WASI glue a library can't provide itself (e.g. mbedTLS entropy/time).
#[derive(Debug, Deserialize)]
pub struct Dep {
    /// Identifier used in the parent's `{dep:NAME:src}` / `{dep:NAME:build}`
    /// placeholders.
    pub name: String,
    /// Upstream URL (tarball or git) — provenance + the fetch hint on error.
    #[serde(default)]
    pub url: String,
    /// Pinned version/ref.
    #[serde(rename = "ref", default)]
    pub git_ref: String,
    /// Git sources: the commit `ref` points at. The fetch checks it.
    #[serde(default)]
    pub commit: Option<String>,
    /// Archive sources: the sha256 of the download. The fetch checks it.
    #[serde(default)]
    pub sha256: Option<String>,
    /// SPDX license expression of the dependency's source.
    pub license: String,
    /// Local source dir, relative to the repo root.
    pub dir: PathBuf,
    /// Build system (currently only `cmake` is supported for deps).
    #[serde(default)]
    pub system: CBuildSystem,
    /// Header include dir relative to `dir` (used when compiling `shims`).
    #[serde(default = "default_include")]
    pub include: PathBuf,
    /// Patch applied to the dep source before building (e.g. config tweaks).
    #[serde(default)]
    pub config_patch: Option<PathBuf>,
    /// Compiler flags (passed via `CMAKE_C_FLAGS`).
    #[serde(default)]
    pub cflags: Vec<String>,
    /// CMake configure flags.
    #[serde(default)]
    pub cmake_flags: Vec<String>,
    /// Extra C sources (repo-root-relative) compiled with this dep's `cflags` +
    /// include dir, then linked into the main build.
    #[serde(default)]
    pub shims: Vec<PathBuf>,
}

fn default_include() -> PathBuf {
    PathBuf::from("include")
}

/// The language lane used to build a CLI.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Rust → wasip2 via cargo.
    Rust,
    /// C/C++ → wasip2 via wasi-sdk.
    C,
    /// Go → wasip3 via the experimental Go fork.
    Go,
}

/// How the C lane builds a CLI.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CBuildSystem {
    /// A single `clang` invocation over an explicit source list
    /// (amalgamation-style, e.g. sqlite3). The default.
    #[default]
    Single,
    /// Configure + build a CMake project with the wasi-sdk toolchain file
    /// (e.g. curl). Uses `cmake_flags` and `artifact`.
    Cmake,
    /// Run an autotools `configure` script cross-compiling to wasip2, then
    /// `make` (e.g. jq). Uses `configure_flags` and `artifact`.
    Autotools,
    /// Run a plain Makefile in the source tree, with the wasi-sdk toolchain
    /// and the build's flags as make variables (e.g. git). Uses `make_flags`,
    /// `make_targets` and `artifact` (relative to the source dir).
    Make,
}

/// Where a CLI's source comes from: a pinned upstream (`repo` plus `commit`
/// or `sha256`, fetched into `dir`), or a local `path`.
#[derive(Debug, Deserialize)]
pub struct Source {
    /// Upstream repository URL (for provenance/reproducibility). Empty for a
    /// `path` source.
    #[serde(default)]
    pub repo: String,
    /// Human-readable label for the pinned version (e.g. a tag). The pin
    /// itself is `commit` or `sha256`. Optional for a `path` source.
    #[serde(rename = "ref", default)]
    pub git_ref: String,
    /// Git sources: the commit `ref` points at. The fetch checks it.
    #[serde(default)]
    pub commit: Option<String>,
    /// Archive sources: the sha256 of the download. The fetch checks it.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Local working copy the driver builds from, relative to the repo root.
    /// For a `path` source, [`Manifest::load`] sets it to the path.
    #[serde(default)]
    pub dir: PathBuf,
    /// A local source tree to build in place instead of a pinned upstream,
    /// for developing a command: relative to the manifest's directory, or
    /// absolute. It can't be combined with `repo`, `commit`, `sha256` or
    /// `dir`, nor with patches (`build.vendor_patch`, a dep's
    /// `config_patch`), which would modify the tree. The build leaves its
    /// usual outputs (`target/`, `vendor/`, objects) in the tree; the Rust
    /// lane builds it without `--locked`. Provenance records it as local, and
    /// `--check` (the publishing gate) refuses it.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// How to build the CLI within its source tree.
#[derive(Debug, Deserialize)]
pub struct Build {
    /// Package/path to build, relative to the source dir (lane-specific meaning).
    /// Used by the Go lane (`go build <package>`).
    #[serde(default = "default_package")]
    pub package: String,
    /// Optional script (repo-root-relative) applied after dependency vendoring
    /// (Go lane) or before compilation (C lane) to stub out code that doesn't
    /// compile for the wasm target.
    #[serde(default)]
    pub vendor_patch: Option<PathBuf>,
    /// C lane: which build system to use (`single`, `cmake`, `autotools` or
    /// `make`).
    #[serde(default)]
    pub system: CBuildSystem,
    /// C lane (`single`): source files to compile, relative to the source dir
    /// (e.g. the SQLite amalgamation's `shell.c`, `sqlite3.c`).
    #[serde(default)]
    pub sources: Vec<PathBuf>,
    /// C lane: compiler flags — defines, optimization, feature toggles
    /// (e.g. `-O2`, `-DSQLITE_THREADSAFE=0`). For `cmake` builds these are
    /// passed via `CMAKE_C_FLAGS`, for `autotools` builds via `CFLAGS`.
    #[serde(default)]
    pub cflags: Vec<String>,
    /// C lane: linker flags (e.g. wasi-sdk emulation libs like
    /// `-lwasi-emulated-signal`). For `cmake` builds these go via
    /// `CMAKE_EXE_LINKER_FLAGS`, for `autotools` builds via `LDFLAGS`.
    #[serde(default)]
    pub link_flags: Vec<String>,
    /// C lane (`cmake`): extra `-D…` flags for the `cmake` configure step.
    #[serde(default)]
    pub cmake_flags: Vec<String>,
    /// C lane (`autotools`): extra flags for the `configure` step (e.g.
    /// `--disable-docs`).
    #[serde(default)]
    pub configure_flags: Vec<String>,
    /// C lane (`cmake`, `autotools`): built artifact path relative to the
    /// build dir (e.g. `src/curl`); copied to `<output>/component.wasm`.
    /// For `make`, relative to the source dir.
    #[serde(default)]
    pub artifact: Option<PathBuf>,
    /// C lane (`make`): extra `make` variable assignments (e.g.
    /// `NO_CURL=YesPlease`), placeholders substituted.
    #[serde(default)]
    pub make_flags: Vec<String>,
    /// C lane (`make`): targets to build (default: make's own default).
    #[serde(default)]
    pub make_targets: Vec<String>,
    /// C lane: extra C sources (repo-root-relative) compiled with the build's
    /// `cflags` and linked into it: WASI glue the project lacks.
    #[serde(default)]
    pub shims: Vec<PathBuf>,
    /// C lane: a WIT world to generate C bindings for with `wit-bindgen c`,
    /// compiled and linked into the build so its sources can call the
    /// world's imports. Generated into `<output>/wit`, so `cflags` can
    /// `-I` it.
    #[serde(default)]
    pub wit: Option<WitBindings>,
    /// Go lane: `go build -ldflags` value. `{ref}` is replaced with the
    /// source's pinned ref and `{version}` with that ref minus a leading `v`,
    /// e.g. to stamp the version a CLI reports.
    #[serde(default)]
    pub ldflags: String,
    /// Go lane: the wasip3 Go fork to build with. Defaults to the fork
    /// eryx-build pins ([`GoToolchain::default`]); set all three fields to
    /// build with another commit (`eryx-build toolchain go <manifest>`
    /// builds it).
    #[serde(default)]
    pub go_toolchain: GoToolchain,
    /// Rust lane: cargo binary to build (`cargo build --bin <bin>`); the wasm
    /// artifact is `<bin>.wasm`. Required for the Rust lane.
    #[serde(default)]
    pub bin: Option<String>,
    /// Rust lane: extra `cargo build` flags (e.g. `--no-default-features`,
    /// `--features foo`).
    #[serde(default)]
    pub cargo_flags: Vec<String>,
    /// Extra environment variables for the build command (e.g.
    /// `RUSTC_BOOTSTRAP = "1"` to let a stable toolchain use an unstable std
    /// feature). Currently honored by the Rust lane.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A WIT world to bind C sources to (see [`Build::wit`]).
#[derive(Debug, Deserialize)]
pub struct WitBindings {
    /// WIT file or directory, relative to the repo root.
    pub path: PathBuf,
    /// The world, e.g. `eryx:process/child`.
    pub world: String,
}

/// A commit of the wasip3 Go fork (see [`Build::go_toolchain`]).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct GoToolchain {
    /// Git URL of the fork.
    pub repo: String,
    /// Branch or tag `commit` is on; a label, like a source's `ref`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The pinned commit (40 hex digits). The toolchain is fetched and
    /// checked at it.
    pub commit: String,
}

impl Default for GoToolchain {
    /// The fork eryx-build builds Go commands with: WASI 0.3.0 as the
    /// wasmtime version eryx uses implements it, with `os.Exit` codes above
    /// 1 kept (eryx-org/eryx-commands#32). This is the one place the default
    /// pin lives; bump all three together.
    fn default() -> Self {
        Self {
            repo: "https://github.com/sd2k/go.git".into(),
            git_ref: "wasip3-0.3.0-go1.27.4-exit-code".into(),
            commit: "331bf58bd2672eda182895f30fd2c688721deba7".into(),
        }
    }
}

fn default_package() -> String {
    ".".to_string()
}

/// Component-model embedding settings.
///
/// Used by the Go lane (its `wasm-tools component embed` step). The C lane emits
/// a component directly from clang, so it ignores this; the table is optional
/// and defaults to `world = "command"` for schema uniformity.
#[derive(Debug, Deserialize)]
pub struct Component {
    /// `wasm-tools component embed` world (e.g. `command`).
    #[serde(default = "default_world")]
    pub world: String,
}

fn default_world() -> String {
    "command".to_string()
}

impl Default for Component {
    fn default() -> Self {
        Self {
            world: default_world(),
        }
    }
}

/// Optional cwasm (pre-compiled) output settings.
#[derive(Debug, Deserialize, Default)]
pub struct Cwasm {
    /// Whether to also pre-compile the component to `.cwasm`.
    #[serde(default)]
    pub enabled: bool,
    /// Extra `wasmtime compile` flags (e.g. the async/stackful knobs Go components need).
    #[serde(default)]
    pub flags: Vec<String>,
}

/// Where the driver writes build artifacts.
#[derive(Debug, Deserialize)]
pub struct Output {
    /// Output directory, relative to the repo root.
    pub dir: PathBuf,
}

impl Manifest {
    /// Load and parse a manifest from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let mut manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        let base = path.parent().unwrap_or(std::path::Path::new(""));
        manifest
            .resolve_source(base)
            .with_context(|| format!("in manifest {}", path.display()))?;
        Ok(manifest)
    }

    /// Check the source's fields. For a `path` source, also point
    /// `source.dir` and `source.path` at the absolute tree (`path` relative
    /// to `base`, the manifest's directory).
    fn resolve_source(&mut self, base: &std::path::Path) -> Result<()> {
        let Some(rel) = &self.source.path else {
            anyhow::ensure!(
                !self.source.dir.as_os_str().is_empty(),
                "`source.dir` is required (or a local `source.path`)"
            );
            return Ok(());
        };
        let src = &self.source;
        anyhow::ensure!(
            src.repo.is_empty()
                && src.commit.is_none()
                && src.sha256.is_none()
                && src.dir.as_os_str().is_empty(),
            "a `path` source can't also set `repo`, `commit`, `sha256` or `dir`"
        );
        anyhow::ensure!(
            self.build.vendor_patch.is_none() && self.deps.iter().all(|d| d.config_patch.is_none()),
            "a `path` source is built in place, so it can't take `vendor_patch` or a dep's \
             `config_patch` (they'd modify the tree); apply the change to the tree instead"
        );
        let abs = std::fs::canonicalize(base.join(rel))
            .with_context(|| format!("source path {}", base.join(rel).display()))?;
        anyhow::ensure!(
            abs.is_dir(),
            "source path {} is not a directory",
            abs.display()
        );
        self.source.dir.clone_from(&abs);
        self.source.path = Some(abs);
        Ok(())
    }

    /// The repo files (repo-root-relative) the build applies, compiles or
    /// links: `build.vendor_patch`, `build.shims`, `build.wit`'s path (a file
    /// or a directory), and each dep's `config_patch` and `shims`.
    pub fn repo_inputs(&self) -> impl Iterator<Item = &PathBuf> {
        self.build
            .vendor_patch
            .iter()
            .chain(&self.build.shims)
            .chain(self.build.wit.iter().map(|w| &w.path))
            .chain(
                self.deps
                    .iter()
                    .flat_map(|d| d.config_patch.iter().chain(&d.shims)),
            )
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A C-lane manifest parses its `[build]` C fields and may omit
    /// `[component]` entirely (it defaults to `world = "command"`).
    #[test]
    fn parses_c_lane_manifest() {
        let toml = r#"
            name = "sqlite3"
            license = "MIT"
            lang = "c"
            [source]
            repo = "https://example.com/sqlite.zip"
            ref = "3.53.2"
            dir = "scratch/sqlite"
            [build]
            sources = ["shell.c", "sqlite3.c"]
            cflags = ["-O2", "-DSQLITE_THREADSAFE=0"]
            link_flags = ["-lwasi-emulated-signal"]
            [output]
            dir = "scratch/sqlite-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("C manifest should parse");
        assert_eq!(m.lang, Lang::C);
        assert_eq!(
            m.build.sources,
            vec![PathBuf::from("shell.c"), PathBuf::from("sqlite3.c")]
        );
        assert_eq!(m.build.cflags, ["-O2", "-DSQLITE_THREADSAFE=0"]);
        assert_eq!(m.build.link_flags, ["-lwasi-emulated-signal"]);
        // No [component] table → defaulted.
        assert_eq!(m.component.world, "command");
        // Default build system is `single`.
        assert_eq!(m.build.system, CBuildSystem::Single);
    }

    /// A C/CMake manifest (curl-style) parses `system = "cmake"` plus its
    /// cmake_flags/artifact fields.
    #[test]
    fn parses_c_cmake_manifest() {
        let toml = r#"
            name = "curl"
            license = "MIT"
            lang = "c"
            [source]
            repo = "https://github.com/curl/curl.git"
            ref = "curl-8_20_0"
            dir = "scratch/curl"
            [build]
            system = "cmake"
            artifact = "src/curl"
            cflags = ["-DPOLLPRI=0"]
            cmake_flags = ["-DCURL_ENABLE_SSL=OFF", "-DBUILD_CURL_EXE=ON"]
            [output]
            dir = "scratch/curl-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("cmake manifest should parse");
        assert_eq!(m.build.system, CBuildSystem::Cmake);
        assert_eq!(m.build.artifact, Some(PathBuf::from("src/curl")));
        assert_eq!(
            m.build.cmake_flags,
            ["-DCURL_ENABLE_SSL=OFF", "-DBUILD_CURL_EXE=ON"]
        );
        // sources is unused in cmake mode and defaults empty.
        assert!(m.build.sources.is_empty());
    }

    /// An autotools manifest (jq-style) parses `system = "autotools"` plus its
    /// configure_flags/artifact fields.
    #[test]
    fn parses_c_autotools_manifest() {
        let toml = r#"
            name = "jq"
            license = "MIT"
            lang = "c"
            [source]
            repo = "https://example.com/jq-1.8.2.tar.gz"
            ref = "1.8.2"
            dir = "scratch/jq"
            [build]
            system = "autotools"
            artifact = "jq"
            configure_flags = ["--disable-docs"]
            [output]
            dir = "scratch/jq-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("autotools manifest should parse");
        assert_eq!(m.build.system, CBuildSystem::Autotools);
        assert_eq!(m.build.artifact, Some(PathBuf::from("jq")));
        assert_eq!(m.build.configure_flags, ["--disable-docs"]);
    }

    /// A plain-Makefile manifest (git-style) parses `system = "make"` plus
    /// its make flags, targets, shims and WIT bindings.
    #[test]
    fn parses_c_make_manifest() {
        let toml = r#"
            name = "git"
            license = "GPL-2.0-only"
            lang = "c"
            [source]
            repo = "https://example.com/git.git"
            ref = "v2.56.0"
            dir = "scratch/git"
            [build]
            system = "make"
            artifact = "git"
            make_targets = ["git"]
            make_flags = ["NO_CURL=YesPlease"]
            shims = ["clis/gpl/git/shims/stubs.c"]
            wit = { path = "crates/eryx-runtime/wit/deps/process.wit", world = "eryx:process/child" }
            [output]
            dir = "scratch/git-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("make manifest should parse");
        assert_eq!(m.build.system, CBuildSystem::Make);
        assert_eq!(m.build.make_targets, ["git"]);
        assert_eq!(m.build.make_flags, ["NO_CURL=YesPlease"]);
        assert_eq!(m.build.shims.len(), 1);
        assert_eq!(m.build.wit.unwrap().world, "eryx:process/child");
    }

    /// A Rust-lane manifest (ripgrep-style) parses its `bin`/`cargo_flags`.
    #[test]
    fn parses_rust_lane_manifest() {
        let toml = r#"
            name = "rg"
            license = "MIT"
            lang = "rust"
            [source]
            repo = "https://github.com/BurntSushi/ripgrep.git"
            ref = "15.1.0"
            dir = "scratch/ripgrep"
            [build]
            bin = "rg"
            cargo_flags = ["--no-default-features"]
            [output]
            dir = "scratch/ripgrep-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("Rust manifest should parse");
        assert_eq!(m.lang, Lang::Rust);
        assert_eq!(m.build.bin.as_deref(), Some("rg"));
        assert_eq!(m.build.cargo_flags, ["--no-default-features"]);
        assert!(m.build.env.is_empty());
    }

    /// A coreutils-style Rust manifest parses `[build.env]` (used to set
    /// RUSTC_BOOTSTRAP for the uutils wasip2 build).
    #[test]
    fn parses_build_env() {
        let toml = r#"
            name = "coreutils"
            license = "MIT"
            lang = "rust"
            [source]
            repo = "https://github.com/uutils/coreutils.git"
            ref = "0.9.0"
            dir = "scratch/uutils"
            [build]
            bin = "coreutils"
            [build.env]
            RUSTC_BOOTSTRAP = "1"
            [output]
            dir = "scratch/coreutils-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("env manifest should parse");
        assert_eq!(
            m.build.env.get("RUSTC_BOOTSTRAP").map(String::as_str),
            Some("1")
        );
    }

    /// A C/CMake manifest with a `[[deps]]` library (curl + mbedTLS) parses the
    /// dep fields and defaults; manifests without deps default to empty.
    #[test]
    fn parses_deps() {
        let toml = r#"
            name = "curl"
            license = "MIT"
            lang = "c"
            [source]
            repo = "https://github.com/curl/curl.git"
            ref = "curl-8_20_0"
            dir = "scratch/curl"
            [build]
            system = "cmake"
            artifact = "src/curl"
            cmake_flags = ["-DMBEDTLS_LIBRARY={dep:mbedtls:build}/library/libmbedtls.a"]
            [[deps]]
            name = "mbedtls"
            license = "Apache-2.0"
            url = "https://example.com/mbedtls-3.6.6.tar.bz2"
            ref = "3.6.6"
            dir = "scratch/mbedtls"
            system = "cmake"
            config_patch = "clis/patches/patch-mbedtls.sh"
            cflags = ["-DMBEDTLS_ENTROPY_HARDWARE_ALT"]
            cmake_flags = ["-DENABLE_TESTING=OFF"]
            shims = ["clis/shims/mbedtls-wasi.c"]
            [output]
            dir = "scratch/curl-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("deps manifest should parse");
        assert_eq!(m.deps.len(), 1);
        let dep = &m.deps[0];
        assert_eq!(dep.name, "mbedtls");
        assert_eq!(dep.system, CBuildSystem::Cmake);
        assert_eq!(dep.include, PathBuf::from("include")); // defaulted
        assert_eq!(dep.shims, vec![PathBuf::from("clis/shims/mbedtls-wasi.c")]);
        assert!(dep.config_patch.is_some());

        // A manifest without [[deps]] defaults to empty.
        let no_deps = r#"
            name = "sqlite3"
            license = "MIT"
            lang = "c"
            [source]
            repo = "x"
            ref = "1"
            dir = "scratch/sqlite"
            [build]
            sources = ["sqlite3.c"]
            [output]
            dir = "out"
        "#;
        let m2: Manifest = toml::from_str(no_deps).expect("parses");
        assert!(m2.deps.is_empty());
    }

    /// A Go-lane manifest still parses, and its C-only `[build]` fields default
    /// to empty (so the schema change is backward-compatible).
    #[test]
    fn go_lane_manifest_leaves_c_fields_empty() {
        let toml = r#"
            name = "gh"
            license = "MIT"
            lang = "go"
            [source]
            repo = "https://github.com/cli/cli.git"
            ref = "v2.87.3"
            dir = "scratch/gh-cli"
            [build]
            package = "./cmd/gh"
            [component]
            world = "command"
            [output]
            dir = "scratch/gh-component"
        "#;
        let m: Manifest = toml::from_str(toml).expect("Go manifest should parse");
        assert_eq!(m.lang, Lang::Go);
        assert_eq!(m.build.package, "./cmd/gh");
        assert!(m.build.sources.is_empty());
        assert!(m.build.cflags.is_empty());
        assert_eq!(m.build.go_toolchain, GoToolchain::default());
    }

    /// `[build.go_toolchain]` overrides the default fork pin, and needs all
    /// of its fields.
    #[test]
    fn go_toolchain_override() {
        let manifest = |table: &str| {
            toml::from_str::<Manifest>(&format!(
                r#"
                name = "gh"
                license = "MIT"
                lang = "go"
                [source]
                repo = "https://github.com/cli/cli.git"
                ref = "v2.87.3"
                dir = "scratch/gh-cli"
                [build]
                package = "./cmd/gh"
                [build.go_toolchain]
                {table}
                [output]
                dir = "scratch/gh-component"
                "#
            ))
        };
        let m = manifest(
            r#"repo = "https://example.com/go.git"
            ref = "test"
            commit = "0123456789012345678901234567890123456789""#,
        )
        .expect("override should parse");
        assert_eq!(m.build.go_toolchain.repo, "https://example.com/go.git");
        assert_eq!(
            m.build.go_toolchain.commit,
            "0123456789012345678901234567890123456789"
        );
        assert!(manifest(r#"commit = "0123456789012345678901234567890123456789""#).is_err());
    }

    /// A `path` source resolves against the manifest's directory and becomes
    /// the build dir; pins, `dir` and patches alongside it are refused.
    #[test]
    fn path_source() {
        let root = std::env::temp_dir().join(format!("eryx-build-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("clis")).unwrap();
        std::fs::create_dir_all(root.join("tool")).unwrap();
        let load = |source: &str, build: &str| {
            let path = root.join("clis/tool.toml");
            std::fs::write(
                &path,
                format!(
                    "name = \"tool\"\nlicense = \"MIT\"\nlang = \"rust\"\n\
                     [source]\n{source}\n[build]\nbin = \"tool\"\n{build}\n\
                     [output]\ndir = \"out\"\n"
                ),
            )
            .unwrap();
            Manifest::load(&path)
        };

        let m = load(r#"path = "../tool""#, "").unwrap();
        let abs = std::fs::canonicalize(root.join("tool")).unwrap();
        assert_eq!(m.source.path.as_deref(), Some(abs.as_path()));
        assert_eq!(m.source.dir, abs);
        assert!(m.source.repo.is_empty());

        let err = |source: &str, build: &str| format!("{:#}", load(source, build).unwrap_err());
        assert!(err(r#"path = "../missing""#, "").contains("source path"));
        for extra in [
            r#"repo = "https://example.com/x.git""#,
            r#"commit = "0000000000000000000000000000000000000000""#,
            r#"dir = "scratch/tool""#,
        ] {
            assert!(
                err(&format!("path = \"../tool\"\n{extra}"), "").contains("can't also set"),
                "{extra}"
            );
        }
        assert!(err(r#"path = "../tool""#, r#"vendor_patch = "p.sh""#).contains("vendor_patch"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
