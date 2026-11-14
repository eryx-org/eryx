//! C/C++ lane: compiles a CLI to a **wasip2 component** via wasi-sdk's
//! `wasm32-wasip2` clang.
//!
//! Unlike the Go lane there is no separate componentization step: wasi-sdk's
//! `wasm32-wasip2` target links via `wasm-component-ld`, so the linked
//! executable is already a component (it imports `wasi:*@0.2.x` and exports
//! `wasi:cli/run`).
//!
//! Two build systems, selected by `[build] system`:
//! - `single` — one `clang` invocation over `[build] sources` (amalgamation
//!   style, e.g. sqlite3).
//! - `cmake` — configure + build a CMake project with the wasi-sdk toolchain
//!   file, then copy `[build] artifact` out of the build dir (e.g. curl).
//!
//! Dependencies: `[[deps]]` libraries (e.g. mbedTLS for curl's TLS) are built
//! first into `<dep.dir>/build`; their `shims` are compiled and linked into the
//! main build, and the main build's flags can reference them via
//! `{dep:NAME:src}` / `{dep:NAME:build}` (and `{repo}`) placeholders.
//!
//! Toolchain input (not per-CLI, so sourced from the environment):
//! - `WASI_SDK_PATH` — root of the wasi-sdk install. The mise `ensure-wasi-sdk`
//!   task installs it on demand and the build tasks export it. It is
//!   deliberately kept OUT of `[tools]`/PATH because a wasm clang on PATH breaks
//!   host proc-macro builds.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::manifest::{CBuildSystem, Dep, Manifest};

pub fn build(manifest: &Manifest) -> Result<()> {
    let repo_root = std::env::current_dir().context("getting current dir")?;

    // Resolve the wasi-sdk (off PATH on purpose — see module docs).
    let sdk = match std::env::var_os("WASI_SDK_PATH") {
        Some(p) => PathBuf::from(p),
        None => bail!(
            "WASI_SDK_PATH not set — install the SDK with `mise run ensure-wasi-sdk` \
             (it stays off PATH; the build-cli task exports it)"
        ),
    };
    let clang = sdk.join("bin/clang");
    if !clang.exists() {
        bail!(
            "clang not found at {} — incomplete wasi-sdk install?",
            clang.display()
        );
    }
    let toolchain = sdk.join("share/cmake/wasi-sdk-p2.cmake");

    let source_dir = repo_root.join(&manifest.source.dir);
    let output_dir = repo_root.join(&manifest.output.dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating output dir {}", output_dir.display()))?;

    // Build dependency libraries first; collect shim objects to link into main.
    let mut shim_objs: Vec<String> = Vec::new();
    for dep in &manifest.deps {
        shim_objs.extend(build_dep(dep, &clang, &toolchain, &repo_root, &output_dir)?);
    }

    // Optional patch hook (mirrors the Go lane), applied to the main source tree
    // before compiling — e.g. working around wasi-libc gaps.
    if let Some(patch) = &manifest.build.vendor_patch {
        eprintln!("=== [{}] patch source tree ===", manifest.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!("vendor_patch script {} not found", patch_abs.display());
        }
        let patch_s = path_str(&patch_abs)?;
        run("bash", &[&patch_s], &source_dir, &[])?;
    }

    let component = output_dir.join("component.wasm");
    match manifest.build.system {
        CBuildSystem::Single => {
            build_single(manifest, &clang, &source_dir, &component, &shim_objs)?;
        }
        CBuildSystem::Cmake => {
            build_cmake(
                manifest,
                &toolchain,
                &source_dir,
                &output_dir,
                &component,
                &repo_root,
                &shim_objs,
            )?;
        }
    }

    compile_cwasm(manifest, &component, &output_dir, &repo_root)?;

    eprintln!("=== [{}] done → {} ===", manifest.name, component.display());
    Ok(())
}

/// Replace `{repo}` and `{dep:NAME:src}` / `{dep:NAME:build}` placeholders with
/// absolute paths, so a manifest can point at built dependency artifacts.
fn substitute(s: &str, deps: &[Dep], repo_root: &Path) -> Result<String> {
    let mut out = s.replace("{repo}", &path_str(repo_root)?);
    for dep in deps {
        let src = repo_root.join(&dep.dir);
        let build = src.join("build");
        out = out
            .replace(&format!("{{dep:{}:src}}", dep.name), &path_str(&src)?)
            .replace(&format!("{{dep:{}:build}}", dep.name), &path_str(&build)?);
    }
    Ok(out)
}

/// Build one `[[deps]]` library (cmake) into `<dir>/build`, then compile its
/// `shims` against it. Returns the shim object paths to link into the main build.
fn build_dep(
    dep: &Dep,
    clang: &Path,
    toolchain: &Path,
    repo_root: &Path,
    output_dir: &Path,
) -> Result<Vec<String>> {
    if dep.system != CBuildSystem::Cmake {
        bail!(
            "dependency '{}' must be system=\"cmake\" (only cmake deps are supported)",
            dep.name
        );
    }
    let dep_src = repo_root.join(&dep.dir);
    if !toolchain.exists() {
        bail!(
            "wasip2 CMake toolchain file not found at {}",
            toolchain.display()
        );
    }

    if let Some(patch) = &dep.config_patch {
        eprintln!("=== [dep:{}] patch source ===", dep.name);
        let patch_abs = repo_root.join(patch);
        if !patch_abs.exists() {
            bail!(
                "dep '{}' config_patch {} not found",
                dep.name,
                patch_abs.display()
            );
        }
        run("bash", &[&path_str(&patch_abs)?], &dep_src, &[])?;
    }

    let build_dir = dep_src.join("build");
    let build_dir_s = path_str(&build_dir)?;
    eprintln!("=== [dep:{}] cmake configure + build ===", dep.name);
    let mut cfg: Vec<String> = vec![
        "-B".into(),
        build_dir_s.clone(),
        "-S".into(),
        path_str(&dep_src)?,
        "-G".into(),
        "Ninja".into(),
        format!("-DCMAKE_TOOLCHAIN_FILE={}", path_str(toolchain)?),
        format!("-DCMAKE_C_FLAGS={}", dep.cflags.join(" ")),
    ];
    cfg.extend(dep.cmake_flags.iter().cloned());
    let cfg_ref: Vec<&str> = cfg.iter().map(String::as_str).collect();
    run("cmake", &cfg_ref, &dep_src, &[])?;
    run("cmake", &["--build", &build_dir_s], &dep_src, &[])?;

    // Compile shims with the dep's cflags + its include dir; link into main later.
    let mut objs = Vec::new();
    if !dep.shims.is_empty() {
        let inc = format!("-I{}", path_str(&dep_src.join(&dep.include))?);
        let clang_s = path_str(clang)?;
        for shim in &dep.shims {
            let shim_abs = repo_root.join(shim);
            if !shim_abs.exists() {
                bail!("dep '{}' shim {} not found", dep.name, shim_abs.display());
            }
            let stem = shim.file_stem().and_then(|s| s.to_str()).unwrap_or("shim");
            let obj = output_dir.join(format!("dep-{}-{stem}.o", dep.name));
            let obj_s = path_str(&obj)?;
            eprintln!("=== [dep:{}] compile shim {} ===", dep.name, shim.display());
            let mut args: Vec<String> = vec!["--target=wasm32-wasip2".into()];
            args.extend(dep.cflags.iter().cloned());
            args.push(inc.clone());
            args.extend([
                "-c".to_string(),
                path_str(&shim_abs)?,
                "-o".to_string(),
                obj_s.clone(),
            ]);
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&clang_s, &args_ref, repo_root, &[])?;
            objs.push(obj_s);
        }
    }
    Ok(objs)
}

/// `single` mode: one `clang --target=wasm32-wasip2` invocation that links a
/// component directly.
fn build_single(
    manifest: &Manifest,
    clang: &Path,
    source_dir: &Path,
    component: &Path,
    shim_objs: &[String],
) -> Result<()> {
    if manifest.build.sources.is_empty() {
        bail!(
            "the C lane (system=single) needs `[build] sources = [...]` for '{}'",
            manifest.name
        );
    }
    eprintln!(
        "=== [{}] compile + link to wasip2 component ===",
        manifest.name
    );
    let clang_s = path_str(clang)?;
    let component_s = path_str(component)?;

    let mut args: Vec<String> = vec!["--target=wasm32-wasip2".to_string()];
    args.extend(manifest.build.cflags.iter().cloned());
    args.extend(
        manifest
            .build
            .sources
            .iter()
            .map(|s| s.to_string_lossy().into_owned()),
    );
    args.extend(manifest.build.link_flags.iter().cloned());
    args.extend(shim_objs.iter().cloned());
    args.push("-o".to_string());
    args.push(component_s);
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&clang_s, &args_ref, source_dir, &[])
}

/// `cmake` mode: configure + build a CMake project with the wasi-sdk wasip2
/// toolchain file, then copy the built artifact to `component.wasm`. Applies
/// `{dep:…}`/`{repo}` placeholder substitution and links any dependency shims.
fn build_cmake(
    manifest: &Manifest,
    toolchain: &Path,
    source_dir: &Path,
    output_dir: &Path,
    component: &Path,
    repo_root: &Path,
    shim_objs: &[String],
) -> Result<()> {
    let artifact_rel = manifest.build.artifact.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the C lane (system=cmake) needs `[build] artifact = \"...\"` for '{}'",
            manifest.name
        )
    })?;

    if !toolchain.exists() {
        bail!(
            "wasip2 CMake toolchain file not found at {}",
            toolchain.display()
        );
    }
    let build_dir = output_dir.join("build");

    // Substitute placeholders in cmake/link flags, then append shim objects.
    let cmake_flags: Vec<String> = manifest
        .build
        .cmake_flags
        .iter()
        .map(|f| substitute(f, &manifest.deps, repo_root))
        .collect::<Result<_>>()?;
    let mut link_parts: Vec<String> = manifest
        .build
        .link_flags
        .iter()
        .map(|f| substitute(f, &manifest.deps, repo_root))
        .collect::<Result<_>>()?;
    link_parts.extend(shim_objs.iter().cloned());

    let toolchain_arg = format!("-DCMAKE_TOOLCHAIN_FILE={}", path_str(toolchain)?);
    let cflags_arg = format!("-DCMAKE_C_FLAGS={}", manifest.build.cflags.join(" "));
    let linker_arg = format!("-DCMAKE_EXE_LINKER_FLAGS={}", link_parts.join(" "));
    let build_dir_s = path_str(&build_dir)?;
    let source_dir_s = path_str(source_dir)?;

    eprintln!("=== [{}] cmake configure ===", manifest.name);
    let mut cfg: Vec<String> = vec![
        "-B".into(),
        build_dir_s.clone(),
        "-S".into(),
        source_dir_s,
        "-G".into(),
        "Ninja".into(),
        toolchain_arg,
        cflags_arg,
        linker_arg,
    ];
    cfg.extend(cmake_flags);
    let cfg_ref: Vec<&str> = cfg.iter().map(String::as_str).collect();
    run("cmake", &cfg_ref, source_dir, &[])?;

    eprintln!("=== [{}] cmake build ===", manifest.name);
    run("cmake", &["--build", &build_dir_s], source_dir, &[])?;

    let artifact = build_dir.join(artifact_rel);
    if !artifact.exists() {
        bail!(
            "expected CMake artifact {} not found after build",
            artifact.display()
        );
    }
    std::fs::copy(&artifact, component)
        .with_context(|| format!("copying {} → {}", artifact.display(), component.display()))?;
    eprintln!("  {} → {}", artifact.display(), component.display());
    Ok(())
}
