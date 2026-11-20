//! Go lane: builds a CLI to wasip3 using the experimental Go fork, then
//! componentizes with `wasm-tools`.
//!
//! The fork targets WASI 0.3.0, as implemented by the wasmtime version eryx
//! uses. Its pin is [`GoToolchain::default`], which a manifest's
//! `[build.go_toolchain]` overrides; [`bootstrap`] (`eryx-build toolchain
//! go`) fetches and builds it.
//!
//! Toolchain inputs (not per-CLI, so sourced from the environment):
//! - `ERYX_GO_WASIP3_ROOT` — GOROOT of the wasip3 Go fork (default
//!   [`default_goroot`]). It must be a checkout at the pinned commit. The
//!   WIT dir is derived from it.
//! - `wasm-tools` and `wasmtime` are taken from `PATH` (mise provides them).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::fetch::{self, Pin};
use crate::manifest::{GoToolchain, Manifest};

/// Wasm features the fork's compiler may use, written to the toolchain's
/// `go.env` so every build with that GOROOT gets them. `widearith`
/// (wide-arithmetic: 128-bit add/sub, 64x64->128 multiply) speeds up
/// math/bits and the crypto behind TLS; wasmtime enables it by default.
const GOWASM: &str = "widearith";

/// Where `eryx-build toolchain go` puts a fork commit unless told otherwise:
/// `$XDG_CACHE_HOME/eryx-build/go-wasip3/<commit>` (or `~/.cache/…`).
pub fn default_goroot(fork: &GoToolchain) -> Result<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(
            std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?,
        )
        .join(".cache"),
    };
    Ok(cache.join("eryx-build/go-wasip3").join(&fork.commit))
}

/// Fetch `fork` into `dir` (shallowly, at its commit) and build it with
/// its own `make.bash` and the host Go, unless `dir` already holds that
/// build. Returns the GOROOT (`dir`, absolute).
///
/// A `dir` holding another commit fails instead of being rebuilt in place:
/// delete it, or use the per-commit default dir.
pub fn bootstrap(fork: &GoToolchain, dir: &Path) -> Result<PathBuf> {
    fetch::fetch(&fork.repo, Pin::Git(&fork.commit), dir)
        .with_context(|| format!("fetching the Go fork ({})", fork.git_ref))?;
    let goroot = std::fs::canonicalize(dir)?;
    if goroot.join("bin/go").exists() {
        eprintln!(
            "=== Go fork @ {} already built in {} ===",
            fork.commit,
            goroot.display()
        );
    } else {
        eprintln!(
            "=== building the Go fork @ {} (make.bash, a few minutes) ===",
            fork.commit
        );
        // make.bash finds the host Go through GOROOT_BOOTSTRAP or PATH. Its
        // log goes to stderr, keeping stdout for the GOROOT.
        let status = std::process::Command::new("bash")
            .arg("make.bash")
            .current_dir(goroot.join("src"))
            .env("GOTOOLCHAIN", "local")
            .stdout(std::io::stderr())
            .status()
            .context("failed to spawn bash")?;
        if !status.success() {
            bail!(
                "the Go fork's make.bash exited with {status} (it needs a host Go on PATH or GOROOT_BOOTSTRAP)"
            );
        }
    }
    // Reset go.env to the fork's and add GOWASM, so re-running after a
    // GOWASM change takes effect without a rebuild.
    run("git", &["checkout", "-q", "--", "go.env"], &goroot, &[])?;
    let go_env = goroot.join("go.env");
    let mut text = std::fs::read_to_string(&go_env)?;
    text.push_str(&format!("\n# Added by eryx-build.\nGOWASM={GOWASM}\n"));
    std::fs::write(&go_env, text)?;
    Ok(goroot)
}

/// The GOROOT to build `manifest` with: `ERYX_GO_WASIP3_ROOT`, else the
/// default dir for its fork. Checked against the fork's pinned commit.
fn goroot(manifest: &Manifest, repo_root: &Path) -> Result<PathBuf> {
    let fork = &manifest.build.go_toolchain;
    let goroot = match std::env::var_os("ERYX_GO_WASIP3_ROOT") {
        Some(p) => repo_root.join(p),
        None => default_goroot(fork)?,
    };
    if !goroot.join("bin/go").exists() {
        bail!(
            "wasip3 Go toolchain not found at {}: build it with `eryx-build toolchain go` \
             (or set ERYX_GO_WASIP3_ROOT)",
            goroot.display()
        );
    }
    fetch::verify_existing(&goroot, Pin::Git(&fork.commit)).with_context(|| {
        format!(
            "the Go toolchain at {} isn't the pinned fork ({}); build it with \
             `eryx-build toolchain go`",
            goroot.display(),
            fork.git_ref
        )
    })?;
    Ok(goroot)
}

pub fn build(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;
    let goroot = goroot(manifest, &repo_root)?;
    let go_bin = goroot.join("bin/go");
    let wit_dir = goroot.join("src/internal/wasi/wit");
    if !wit_dir.exists() {
        bail!("WIT dir not found at {}", wit_dir.display());
    }

    let source_dir = repo_root.join(&manifest.source.dir);
    let output_dir = repo_root.join(&manifest.output.dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    let goroot_s = path_str(&goroot)?;
    let go_bin_s = path_str(&go_bin)?;
    let wit_s = path_str(&wit_dir)?;
    let out_wasm = output_dir.join(format!("{}.wasm", manifest.name));
    let out_wasm_s = path_str(&out_wasm)?;

    eprintln!("=== [{}] Step 1: vendor dependencies ===", manifest.name);
    run(
        &go_bin_s,
        &["mod", "vendor"],
        &source_dir,
        &[("GOROOT", &goroot_s)],
    )?;

    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] Step 2: patch vendor tree ===", manifest.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!("vendor_patch script {} not found", patch_abs.display());
        }
        let patch_s = path_str(&patch_abs)?;
        run("bash", &[&patch_s], &source_dir, &[])?;
    }

    eprintln!(
        "=== [{}] Step 3: compile to wasm32 (wasip3) ===",
        manifest.name
    );
    let ldflags = manifest
        .build
        .ldflags
        .replace("{ref}", &manifest.source.git_ref)
        .replace("{version}", manifest.source.git_ref.trim_start_matches('v'));
    let mut args = vec!["build", "-mod=vendor", "-o", &out_wasm_s];
    if !ldflags.is_empty() {
        args.extend(["-ldflags", &ldflags]);
    }
    args.push(&manifest.build.package);
    run(
        &go_bin_s,
        &args,
        &source_dir,
        &[
            ("GOROOT", &goroot_s),
            ("GOOS", "wasip3"),
            ("GOARCH", "wasm32"),
        ],
    )?;

    eprintln!("=== [{}] Step 4: componentize ===", manifest.name);
    let embedded = output_dir.join("embedded.wasm");
    let component = output_dir.join("component.wasm");
    let embedded_s = path_str(&embedded)?;
    let component_s = path_str(&component)?;
    run(
        "wasm-tools",
        &[
            "component",
            "embed",
            "--all-features",
            &wit_s,
            "--world",
            &manifest.component.world,
            &out_wasm_s,
            "-o",
            &embedded_s,
        ],
        &repo_root,
        &[],
    )?;
    run(
        "wasm-tools",
        &["component", "new", &embedded_s, "-o", &component_s],
        &repo_root,
        &[],
    )?;
    std::fs::remove_file(&embedded).ok();

    compile_cwasm(manifest, &component, &output_dir, &repo_root)?;

    eprintln!("=== [{}] done → {} ===", manifest.name, component.display());
    Ok(())
}
