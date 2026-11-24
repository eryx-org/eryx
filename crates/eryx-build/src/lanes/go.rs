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
//!
//! The lane always sets `GOCACHE` to a per-commit dir ([`gocache`]),
//! overriding any inherited one.
//!
//! With `[build] std_tool`, the source is a second checkout of the fork, at
//! the toolchain's commit, and the lane builds one of Go's own commands
//! (`cmd/go`, `cmd/compile`, ...) from it: Go builds those only from their
//! GOROOT. What the toolchain's `make.bash` generated under `pkg/`, and its
//! `go.env`, go into the checkout ([`prepare_std_tree`]), so the checkout's
//! own `make.bash` never runs.
//!
//! With `[build] wit`, the component's world also includes that world
//! ([`component_wit`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::fetch::{self, Pin};
use crate::manifest::{GoToolchain, Manifest, WitBindings};

/// Wasm features the fork's compiler may use, written to the toolchain's
/// `go.env` so every build with that GOROOT gets them. `widearith`
/// (wide-arithmetic: 128-bit add/sub, 64x64->128 multiply) speeds up
/// math/bits and the crypto behind TLS; wasmtime enables it by default.
const GOWASM: &str = "widearith";

/// `$XDG_CACHE_HOME/eryx-build/<sub>/<commit>` (or `~/.cache/…`).
fn cache_dir(sub: &str, fork: &GoToolchain) -> Result<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(
            std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?,
        )
        .join(".cache"),
    };
    Ok(cache.join("eryx-build").join(sub).join(&fork.commit))
}

/// Where `eryx-build toolchain go` puts a fork commit unless told otherwise:
/// `$XDG_CACHE_HOME/eryx-build/go-wasip3/<commit>` (or `~/.cache/…`).
pub fn default_goroot(fork: &GoToolchain) -> Result<PathBuf> {
    cache_dir("go-wasip3", fork)
}

/// The `GOCACHE` the lane builds with: one per fork commit. Every commit of
/// the fork reports the same version (`go1.27.4`), which Go's build cache
/// keys on, so a shared cache would reuse objects from the old compiler
/// after a pin bump.
fn gocache(fork: &GoToolchain) -> Result<PathBuf> {
    cache_dir("go-build", fork)
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

    let std_tool = manifest.build.std_tool;
    // A Go command builds from its GOROOT: the source tree, for std_tool.
    let build_root = if std_tool {
        std::fs::canonicalize(&source_dir)?
    } else {
        goroot.clone()
    };
    let build_root_s = path_str(&build_root)?;
    let gocache_s = path_str(&gocache(&manifest.build.go_toolchain)?)?;
    let go_bin_s = path_str(&go_bin)?;
    let out_wasm = output_dir.join(format!("{}.wasm", manifest.name));
    let out_wasm_s = path_str(&out_wasm)?;

    if std_tool {
        eprintln!(
            "=== [{}] Step 1: use the Go fork's tree as GOROOT ===",
            manifest.name
        );
        prepare_std_tree(&goroot, &build_root)?;
    } else {
        eprintln!("=== [{}] Step 1: vendor dependencies ===", manifest.name);
        run(
            &go_bin_s,
            &["mod", "vendor"],
            &source_dir,
            &[("GOROOT", &build_root_s), ("GOCACHE", &gocache_s)],
        )?;
    }

    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] Step 2: patch the source tree ===", manifest.name);
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
    // Go's own commands build from the std tree, which vendors for itself.
    let mut args = vec!["build"];
    if !std_tool {
        args.push("-mod=vendor");
    }
    args.extend(["-o", &out_wasm_s]);
    if !ldflags.is_empty() {
        args.extend(["-ldflags", &ldflags]);
    }
    args.push(&manifest.build.package);
    let build_dir = if std_tool {
        build_root.join("src")
    } else {
        source_dir.clone()
    };
    run(
        &go_bin_s,
        &args,
        &build_dir,
        &[
            ("GOROOT", &build_root_s),
            ("GOCACHE", &gocache_s),
            ("GOOS", "wasip3"),
            ("GOARCH", "wasm32"),
        ],
    )?;

    eprintln!("=== [{}] Step 4: componentize ===", manifest.name);
    let (wit, world) = component_wit(&wit_dir, manifest, &output_dir, &repo_root)?;
    let wit_s = path_str(&wit)?;
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
            &world,
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

/// Make `tree`, a checkout of the fork at the toolchain's commit, usable as
/// a GOROOT for the toolchain's `go`, by giving it what the toolchain's
/// `make.bash` generated: the sources under `src/` that git ignores
/// (`internal/buildcfg/zbootstrap.go` and the like), copied; `pkg/tool` (the
/// native tools) and `pkg/include` (the assembler's headers), linked; and
/// `go.env`, which sets `GOWASM`, copied. Both commits match, so these are
/// what `make.bash` would make there.
fn prepare_std_tree(goroot: &Path, tree: &Path) -> Result<()> {
    let out = std::process::Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            "src",
        ])
        .current_dir(goroot)
        .output()
        .context("running git ls-files in the Go toolchain")?;
    if !out.status.success() {
        bail!("git ls-files in {} failed", goroot.display());
    }
    for file in out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let file = std::str::from_utf8(file).context("non-UTF-8 path in the Go toolchain")?;
        std::fs::copy(goroot.join(file), tree.join(file))
            .with_context(|| format!("copying the toolchain's {file}"))?;
    }
    std::fs::create_dir_all(tree.join("pkg"))?;
    for sub in ["pkg/tool", "pkg/include"] {
        let link = tree.join(sub);
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.is_symlink() => std::fs::remove_file(&link)?,
            Ok(_) => bail!(
                "{} exists and isn't a link into the toolchain",
                link.display()
            ),
            Err(_) => {}
        }
        symlink(&goroot.join(sub), &link).with_context(|| format!("linking {}", link.display()))?;
    }
    std::fs::copy(goroot.join("go.env"), tree.join("go.env"))
        .with_context(|| format!("copying go.env into {}", tree.display()))?;
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, link)?)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> Result<()> {
    bail!("`std_tool` builds need a Unix host")
}

/// The WIT and world to embed the component with: the fork's WIT and the
/// manifest's `[component] world`, or, with `[build] wit`, a copy of the
/// fork's WIT in `<output>/wit` with that WIT added as a dependency and a
/// world including both.
fn component_wit(
    fork_wit: &Path,
    manifest: &Manifest,
    output_dir: &Path,
    repo_root: &Path,
) -> Result<(PathBuf, String)> {
    let Some(extra) = &manifest.build.wit else {
        return Ok((fork_wit.to_path_buf(), manifest.component.world.clone()));
    };
    let out = output_dir.join("wit");
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    copy_dir(fork_wit, &out)?;
    add_world(
        &out,
        &repo_root.join(&extra.path),
        &manifest.component.world,
        extra,
    )
}

/// The world [`component_wit`] embeds with.
const COMBINED_WORLD: &str = "eryx-build-command";

/// Add `extra_path` (a WIT file or dir) to `wit`'s `deps/`, and a world to
/// `wit/worlds.wit` that includes `world` and `extra.world`.
fn add_world(
    wit: &Path,
    extra_path: &Path,
    world: &str,
    extra: &WitBindings,
) -> Result<(PathBuf, String)> {
    let name = extra_path
        .file_name()
        .with_context(|| format!("{} has no file name", extra_path.display()))?;
    let dest = wit.join("deps").join(name);
    if extra_path.is_dir() {
        copy_dir(extra_path, &dest)?;
    } else {
        std::fs::create_dir_all(wit.join("deps"))?;
        std::fs::copy(extra_path, &dest)
            .with_context(|| format!("copying {}", extra_path.display()))?;
    }
    let worlds = wit.join("worlds.wit");
    let mut text = std::fs::read_to_string(&worlds)
        .with_context(|| format!("reading {}", worlds.display()))?;
    text.push_str(&format!(
        "\n// Added by eryx-build for `[build] wit`.\nworld {COMBINED_WORLD} {{\n  \
         include {world};\n  include {};\n}}\n",
        extra.world
    ));
    std::fs::write(&worlds, text)?;
    Ok((wit.to_path_buf(), COMBINED_WORLD.to_string()))
}

/// Copy the directory `from` to `to`, recursively.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `[build] wit` adds its WIT as a dependency of a copy of the fork's,
    /// and a world including both worlds.
    #[test]
    fn add_world_includes_both() {
        let root = std::env::temp_dir().join(format!("eryx-build-wit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let wit = root.join("wit");
        std::fs::create_dir_all(wit.join("deps")).unwrap();
        std::fs::write(wit.join("worlds.wit"), "package go:wasip3-proto;\n").unwrap();
        let extra_path = root.join("process.wit");
        std::fs::write(&extra_path, "package eryx:process@0.1.0;\n").unwrap();
        let extra = WitBindings {
            path: extra_path.clone(),
            world: "eryx:process/child".into(),
        };

        let (dir, world) = add_world(&wit, &extra_path, "command", &extra).unwrap();
        assert_eq!(dir, wit);
        assert_eq!(world, COMBINED_WORLD);
        assert_eq!(
            std::fs::read_to_string(wit.join("deps/process.wit")).unwrap(),
            "package eryx:process@0.1.0;\n"
        );
        let worlds = std::fs::read_to_string(wit.join("worlds.wit")).unwrap();
        assert!(worlds.contains(
            "world eryx-build-command {\n  include command;\n  include eryx:process/child;\n}"
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
