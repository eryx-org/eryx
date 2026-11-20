//! Rust lane: builds a CLI to a **wasip2 component** via `cargo build
//! --target wasm32-wasip2`.
//!
//! The simplest lane: since Rust 1.82 the `wasm32-wasip2` target emits a
//! component directly (no `wasm-tools` step, no wasi-sdk). The spike candidate
//! (ripgrep) built and ran clean with no source patches or special flags —
//! `cargo build --release --target wasm32-wasip2 --bin <bin>` and the artifact
//! is `target/wasm32-wasip2/release/<bin>.wasm`.
//!
//! Every build links `src/exit_code.rs` with `--wrap=__main_void`, so an
//! `ExitCode` returned from `main` survives (#1131).
//!
//! The `wasm32-wasip2` rustup target must be installed
//! (`rustup target add wasm32-wasip2`). cargo comes from PATH.

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run, rust_obj};
use crate::manifest::Manifest;

pub fn build(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;

    let bin = manifest.build.bin.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "the Rust lane needs `[build] bin = \"...\"` (the cargo binary) for '{}'",
            manifest.name
        )
    })?;

    let source_dir = repo_root.join(&manifest.source.dir);
    let output_dir = repo_root.join(&manifest.output.dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    // Optional patch hook (mirrors the other lanes), applied before building.
    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] patch source tree ===", manifest.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!("vendor_patch script {} not found", patch_abs.display());
        }
        let patch_s = path_str(&patch_abs)?;
        run("bash", &[&patch_s], &source_dir, &[])?;
    }

    let cwd_obj = super::cwd_from_pwd_obj(&output_dir)?;
    let exit_obj = rust_obj(&output_dir, "exit_code", include_str!("../exit_code.rs"))?;
    eprintln!("=== [{}] cargo build → wasip2 component ===", manifest.name);
    // `cargo rustc`, to link the start-in-$PWD constructor (#978) and the
    // exit status wrapper (#1131) into the binary alone without replacing
    // the project's own RUSTFLAGS.
    // `cargo rustc` needs the package, which a workspace root (jaq's) doesn't
    // imply.
    // Build exactly the pinned lockfile (after any vendor_patch updates it),
    // except for a local `path` source under development.
    let locked = manifest.source.path.is_none();
    let package = package_of(&source_dir, bin, locked)?;
    let mut args: Vec<String> = vec![
        "rustc".into(),
        "--package".into(),
        package,
        "--release".into(),
        "--target".into(),
        "wasm32-wasip2".into(),
        "--bin".into(),
        bin.to_string(),
    ];
    if locked {
        args.push("--locked".into());
    }
    args.extend(manifest.build.cargo_flags.iter().cloned());
    args.extend(["--".into()]);
    args.extend(link_args(&cwd_obj, &exit_obj));
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    // Pin the target dir to the source tree: a `.cargo/config.toml` or
    // CARGO_TARGET_DIR inherited from the eryx checkout would otherwise send
    // the build (and the artifact this lane looks for) somewhere else.
    let target_dir = source_dir.join("target");
    let target_dir_s = path_str(&target_dir)?;
    let mut envs: Vec<(&str, &str)> = vec![("CARGO_TARGET_DIR", &target_dir_s)];
    envs.extend(
        manifest
            .build
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    run("cargo", &args_ref, &source_dir, &envs)?;

    let artifact = target_dir
        .join("wasm32-wasip2/release")
        .join(format!("{bin}.wasm"));
    if !artifact.exists() {
        bail!(
            "expected cargo artifact {} not found after build",
            artifact.display()
        );
    }
    let component = output_dir.join("component.wasm");
    std::fs::copy(&artifact, &component)
        .with_context(|| format!("copying {} → {}", artifact.display(), component.display()))?;
    eprintln!("  {} → {}", artifact.display(), component.display());

    compile_cwasm(manifest, &component, &output_dir, &repo_root)?;

    eprintln!("=== [{}] done → {} ===", manifest.name, component.display());
    Ok(())
}

/// `rustc` flags linking the start-up objects into the binary.
fn link_args(cwd_obj: &str, exit_obj: &str) -> Vec<String> {
    [cwd_obj, exit_obj, "--wrap=__main_void"]
        .into_iter()
        .flat_map(|arg| ["-C".into(), format!("link-arg={arg}")])
        .collect()
}

/// The package in the workspace at `source_dir` that has the binary `bin`,
/// read with `--locked` when `locked`.
fn package_of(source_dir: &std::path::Path, bin: &str, locked: bool) -> Result<String> {
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .args(locked.then_some("--locked"))
        .current_dir(source_dir)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
    let packages = metadata["packages"].as_array().into_iter().flatten();
    for package in packages {
        let has_bin = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| {
                t["name"] == bin
                    && t["kind"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k == "bin"))
            });
        if has_bin && let Some(name) = package["name"].as_str() {
            return Ok(name.to_owned());
        }
    }
    bail!(
        "no package in {} has a binary named '{bin}'",
        source_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `ExitCode` returned from `main` survives, as does
    /// `process::exit`'s. Opt-in: it needs the `wasm32-wasip2` Rust target,
    /// and `wasmtime` on PATH (`mise exec --` provides it).
    #[test]
    #[ignore = "needs the wasm32-wasip2 target and wasmtime"]
    fn exit_statuses_survive() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("eryx-build-rust-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("t.rs"),
            "fn main() -> std::process::ExitCode {\n\
                 let args: Vec<String> = std::env::args().collect();\n\
                 if let Some(n) = args.get(2) { std::process::exit(n.parse().unwrap_or(99)); }\n\
                 std::process::ExitCode::from(args[1].parse::<u8>().unwrap_or(99))\n\
             }\n",
        )?;
        let cwd_obj = crate::lanes::cwd_from_pwd_obj(&dir)?;
        let exit_obj = rust_obj(&dir, "exit_code", include_str!("../exit_code.rs"))?;
        let component = dir.join("t.wasm");
        let status = std::process::Command::new("rustc")
            .args(["--edition", "2024", "--target", "wasm32-wasip2", "-O", "-o"])
            .arg(&component)
            .arg(dir.join("t.rs"))
            .args(link_args(&cwd_obj, &exit_obj))
            .status()?;
        anyhow::ensure!(status.success(), "rustc exited with {status}");
        let cases: [(&[&str], i32); 5] = [
            (&["0"], 0),
            (&["3"], 3),
            (&["255"], 255),
            (&["0", "7"], 7),
            (&["0", "255"], 255),
        ];
        for (args, want) in cases {
            let status = std::process::Command::new("wasmtime")
                .arg("run")
                .arg(&component)
                .args(args)
                .status()?;
            assert_eq!(status.code(), Some(want), "t {args:?}");
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
