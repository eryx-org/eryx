//! C/C++ lane: compiles a CLI to a **wasip2 component** via wasi-sdk's
//! `wasm32-wasip2` clang.
//!
//! Unlike the Go lane there is no separate componentization step: wasi-sdk's
//! `wasm32-wasip2` target links via `wasm-component-ld`, so the linked
//! executable is already a component (it imports `wasi:*@0.2.x` and exports
//! `wasi:cli/run`).
//!
//! The build systems, selected by `[build] system`:
//! - `single` — one `clang` invocation over `[build] sources` (amalgamation
//!   style, e.g. sqlite3).
//! - `cmake` — configure + build a CMake project with the wasi-sdk toolchain
//!   file, then copy `[build] artifact` out of the build dir (e.g. curl).
//! - `autotools` — run the project's `configure` cross-compiling to wasip2
//!   with wasi-sdk's clang, then `make`, and copy `[build] artifact` out of
//!   the build dir (e.g. jq).
//! - `make` — run a plain Makefile in the source tree with wasi-sdk's clang
//!   and the build's flags as make variables, and copy `[build] artifact`
//!   out (e.g. git).
//!
//! Every build links `src/exit_code.c`, so exit statuses above 1 survive
//! (wasi-libc on wasip2 reports only success or failure).
//!
//! `[build] shims` are compiled with the build's own `cflags` and linked in,
//! and `[build] wit` generates `wit-bindgen c` bindings into `<output>/wit`
//! (compiled and linked the same way) for sources that call component
//! imports.
//!
//! Dependencies: `[[deps]]` libraries (e.g. mbedTLS for curl's TLS) are built
//! first, in order, into `<dep.dir>/build`; their `shims` are compiled and
//! linked into the main build, and the main build's flags can reference them
//! via `{dep:NAME:src}` / `{dep:NAME:build}` (and `{repo}`) placeholders. So
//! can a dep's own `cflags` and `cmake_flags`, for the deps listed before it
//! (e.g. libcurl built against mbedTLS).
//!
//! Toolchain input (not per-CLI, so sourced from the environment):
//! - `WASI_SDK_PATH` — root of the wasi-sdk install. The mise `ensure-wasi-sdk`
//!   task installs it on demand and the build tasks export it. It is
//!   deliberately kept OUT of `[tools]`/PATH because a wasm clang on PATH breaks
//!   host proc-macro builds.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{compile_cwasm, path_str, run};
use crate::manifest::{CBuildSystem, Dep, Manifest, WitBindings};

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
    for (i, dep) in manifest.deps.iter().enumerate() {
        let earlier = &manifest.deps[..i];
        shim_objs.extend(build_dep(
            dep,
            earlier,
            &clang,
            &toolchain,
            &repo_root,
            &output_dir,
        )?);
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

    // The main build's own glue: the start-in-$PWD constructor, the exit
    // status shim, WIT bindings, then shims.
    shim_objs.push(super::cwd_from_pwd_obj(&output_dir)?);
    shim_objs.extend(exit_code_objs(manifest, &clang, &output_dir)?);
    if let Some(wit) = &manifest.build.wit {
        shim_objs.extend(build_wit(manifest, wit, &clang, &output_dir, &repo_root)?);
    }
    shim_objs.extend(build_shims(manifest, &clang, &output_dir, &repo_root)?);

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
        CBuildSystem::Make => {
            build_make(
                manifest,
                &sdk,
                &source_dir,
                &component,
                &repo_root,
                &shim_objs,
            )?;
        }
        CBuildSystem::Autotools => {
            build_autotools(
                manifest,
                &sdk,
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

/// `dep`'s `cflags` and `cmake_flags`, with placeholders for the `earlier`
/// deps (those built before it) substituted. A placeholder for any other dep
/// is an error, since that one isn't built yet.
fn dep_flags(dep: &Dep, earlier: &[Dep], repo_root: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let resolve = |flags: &[String]| -> Result<Vec<String>> {
        flags
            .iter()
            .map(|flag| {
                let flag = substitute(flag, earlier, repo_root)?;
                if flag.contains("{dep:") {
                    bail!(
                        "dependency '{}' flag `{flag}` refers to a dependency that isn't built \
                         before it (list it earlier in [[deps]])",
                        dep.name
                    );
                }
                Ok(flag)
            })
            .collect()
    };
    Ok((resolve(&dep.cflags)?, resolve(&dep.cmake_flags)?))
}

/// Build one `[[deps]]` library (cmake) into `<dir>/build`, then compile its
/// `shims` against it. Its flags may refer to the `earlier` deps. Returns the
/// shim object paths to link into the main build.
fn build_dep(
    dep: &Dep,
    earlier: &[Dep],
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
    let (cflags, cmake_flags) = dep_flags(dep, earlier, repo_root)?;
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
        format!("-DCMAKE_C_FLAGS={}", cflags.join(" ")),
    ];
    cfg.extend(cmake_flags);
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
            args.extend(cflags.iter().cloned());
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

    copy_artifact(&build_dir.join(artifact_rel), component)
}

/// `autotools` mode: run the project's `configure` out of tree in
/// `<output>/build`, cross-compiling to wasip2 with wasi-sdk's clang, then
/// `make` and copy the built artifact to `component.wasm`. `cflags` and
/// `link_flags` (with `{dep:…}`/`{repo}` substituted, plus dependency shims)
/// become `CFLAGS` and `LDFLAGS`, so `configure`'s feature probes see them.
fn build_autotools(
    manifest: &Manifest,
    sdk: &Path,
    source_dir: &Path,
    output_dir: &Path,
    component: &Path,
    repo_root: &Path,
    shim_objs: &[String],
) -> Result<()> {
    let artifact_rel = manifest.build.artifact.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the C lane (system=autotools) needs `[build] artifact = \"...\"` for '{}'",
            manifest.name
        )
    })?;
    let subst = |flags: &[String]| -> Result<Vec<String>> {
        flags
            .iter()
            .map(|f| substitute(f, &manifest.deps, repo_root))
            .collect()
    };
    let cflags = subst(&manifest.build.cflags)?.join(" ");
    let mut ldflags = subst(&manifest.build.link_flags)?;
    // As `-Wl,` linker inputs: libtool refuses a bare object in LDFLAGS when
    // it links a library (jq's libjq.la), and passes these through.
    ldflags.extend(shim_objs.iter().map(|obj| {
        if obj.starts_with("-Wl,") {
            obj.clone()
        } else {
            format!("-Wl,{obj}")
        }
    }));
    let ldflags = ldflags.join(" ");
    let configure_flags = subst(&manifest.build.configure_flags)?;

    let build_dir = output_dir.join("build");
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("creating build dir {}", build_dir.display()))?;
    let bin = |tool: &str| path_str(&sdk.join("bin").join(tool));
    let cc = format!("{} --target=wasm32-wasip2", bin("clang")?);
    let (ar, ranlib) = (bin("llvm-ar")?, bin("llvm-ranlib")?);
    let env = [
        ("CC", cc.as_str()),
        ("AR", ar.as_str()),
        ("RANLIB", ranlib.as_str()),
        ("CFLAGS", cflags.as_str()),
        ("LDFLAGS", ldflags.as_str()),
    ];

    eprintln!("=== [{}] configure ===", manifest.name);
    let configure = path_str(&source_dir.join("configure"))?;
    // autotools' config.sub knows `wasm32-wasi`, not `wasm32-wasip2`; the
    // target clang builds for comes from `CC`.
    let mut args = vec!["--host=wasm32-wasi"];
    args.extend(configure_flags.iter().map(String::as_str));
    run(&configure, &args, &build_dir, &env)?;

    eprintln!("=== [{}] make ===", manifest.name);
    let jobs = format!(
        "-j{}",
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    );
    run("make", &[&jobs], &build_dir, &[])?;

    copy_artifact(&build_dir.join(artifact_rel), component)
}

/// Compile one C source with the build's `cflags` (placeholders substituted)
/// to `<output>/<name>.o`.
fn compile_c(
    manifest: &Manifest,
    clang: &Path,
    source: &Path,
    name: &str,
    extra: &[String],
    output_dir: &Path,
    repo_root: &Path,
) -> Result<String> {
    let obj = path_str(&output_dir.join(format!("{name}.o")))?;
    let mut args: Vec<String> = vec!["--target=wasm32-wasip2".into()];
    for flag in &manifest.build.cflags {
        args.push(substitute(flag, &manifest.deps, repo_root)?);
    }
    args.extend(extra.iter().cloned());
    args.extend(["-c".into(), path_str(source)?, "-o".into(), obj.clone()]);
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&path_str(clang)?, &args_ref, repo_root, &[])?;
    Ok(obj)
}

/// Build `src/exit_code.c`, which passes the program's exit status on with
/// `exit-with-code` (wasi-libc only reports success or failure); returns its
/// object, the component-type object declaring the import, and the
/// `--wrap=__main_void` flag it relies on, all for the link line.
fn exit_code_objs(manifest: &Manifest, clang: &Path, output_dir: &Path) -> Result<Vec<String>> {
    eprintln!("=== [{}] compile exit status shim ===", manifest.name);
    let dir = output_dir.join("exit-code");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let source = dir.join("exit_code.c");
    let wit = dir.join("exit_code.wit");
    std::fs::write(&source, include_str!("../exit_code.c"))
        .with_context(|| format!("writing {}", source.display()))?;
    std::fs::write(&wit, include_str!("../exit_code.wit"))
        .with_context(|| format!("writing {}", wit.display()))?;
    let dir_s = path_str(&dir)?;
    run(
        "wit-bindgen",
        &[
            "c",
            "--world",
            "eryx-exit-code",
            &path_str(&wit)?,
            "--out-dir",
            &dir_s,
        ],
        &dir,
        &[],
    )
    .context("the C lane needs `wit-bindgen` for its exit status shim")?;
    let obj = path_str(&dir.join("exit_code.o"))?;
    run(
        &path_str(clang)?,
        &[
            "--target=wasm32-wasip2",
            "-O2",
            "-c",
            &path_str(&source)?,
            "-o",
            &obj,
        ],
        &dir,
        &[],
    )?;
    let component_type = path_str(&dir.join("eryx_exit_code_component_type.o"))?;
    Ok(vec![obj, component_type, "-Wl,--wrap=__main_void".into()])
}

/// Compile `[build] shims`; returns their objects.
fn build_shims(
    manifest: &Manifest,
    clang: &Path,
    output_dir: &Path,
    repo_root: &Path,
) -> Result<Vec<String>> {
    let mut objs = Vec::new();
    for shim in &manifest.build.shims {
        let shim_abs = repo_root.join(shim);
        if !shim_abs.exists() {
            bail!("shim {} not found", shim_abs.display());
        }
        let stem = shim.file_stem().and_then(|s| s.to_str()).unwrap_or("shim");
        eprintln!(
            "=== [{}] compile shim {} ===",
            manifest.name,
            shim.display()
        );
        let name = format!("shim-{stem}");
        objs.push(compile_c(
            manifest,
            clang,
            &shim_abs,
            &name,
            &[],
            output_dir,
            repo_root,
        )?);
    }
    Ok(objs)
}

/// Generate `wit-bindgen c` bindings for `wit` into `<output>/wit` and
/// compile them; returns the objects to link (the bindings and the
/// component-type object `wasm-component-ld` reads the world from).
fn build_wit(
    manifest: &Manifest,
    wit: &WitBindings,
    clang: &Path,
    output_dir: &Path,
    repo_root: &Path,
) -> Result<Vec<String>> {
    let out = output_dir.join("wit");
    let out_s = path_str(&out)?;
    let wit_path = path_str(&repo_root.join(&wit.path))?;
    eprintln!("=== [{}] wit-bindgen c {} ===", manifest.name, wit.world);
    run(
        "wit-bindgen",
        &["c", "--world", &wit.world, &wit_path, "--out-dir", &out_s],
        repo_root,
        &[],
    )
    .with_context(|| {
        // An older wit-bindgen elsewhere on PATH can't parse async WIT.
        let found = std::process::Command::new("wit-bindgen")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        format!("wit-bindgen c failed (found {found:?}; mise.toml pins the version that works)")
    })?;
    // wit-bindgen names its files after the world's name.
    let world = wit.world.rsplit('/').next().unwrap_or(&wit.world);
    let source = out.join(format!("{world}.c"));
    let obj = compile_c(
        manifest,
        clang,
        &source,
        &format!("wit-{world}"),
        &[format!("-I{out_s}")],
        output_dir,
        repo_root,
    )?;
    let component_type = path_str(&out.join(format!("{world}_component_type.o")))?;
    Ok(vec![obj, component_type])
}

/// `make` mode: run the project's Makefile in its source tree, with
/// wasi-sdk's clang and tools, `cflags` as `CFLAGS`, `link_flags` (plus shim
/// objects) as `LDFLAGS` and `make_flags` as further variables, all with
/// placeholders substituted, then copy the artifact to `component.wasm`.
fn build_make(
    manifest: &Manifest,
    sdk: &Path,
    source_dir: &Path,
    component: &Path,
    repo_root: &Path,
    shim_objs: &[String],
) -> Result<()> {
    let artifact_rel = manifest.build.artifact.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the C lane (system=make) needs `[build] artifact = \"...\"` for '{}'",
            manifest.name
        )
    })?;
    let subst = |flags: &[String]| -> Result<Vec<String>> {
        flags
            .iter()
            .map(|f| substitute(f, &manifest.deps, repo_root))
            .collect()
    };
    let mut ldflags = subst(&manifest.build.link_flags)?;
    ldflags.extend(shim_objs.iter().cloned());
    let bin = |tool: &str| path_str(&sdk.join("bin").join(tool));
    let mut args = vec![
        format!(
            "-j{}",
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        ),
        format!("CC={} --target=wasm32-wasip2", bin("clang")?),
        format!("AR={}", bin("llvm-ar")?),
        format!("RANLIB={}", bin("llvm-ranlib")?),
        format!("CFLAGS={}", subst(&manifest.build.cflags)?.join(" ")),
        format!("LDFLAGS={}", ldflags.join(" ")),
    ];
    args.extend(subst(&manifest.build.make_flags)?);
    args.extend(manifest.build.make_targets.iter().cloned());
    // The Makefile tracks flags, not the shim and WIT objects they name, so
    // relink every time rather than keep an artifact built against old ones.
    let artifact = source_dir.join(artifact_rel);
    if artifact.exists() {
        std::fs::remove_file(&artifact)
            .with_context(|| format!("removing stale {}", artifact.display()))?;
    }
    eprintln!("=== [{}] make ===", manifest.name);
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run("make", &args_ref, source_dir, &[])?;
    copy_artifact(&artifact, component)
}

/// Copy a built executable (already a component) to `component.wasm`.
fn copy_artifact(artifact: &Path, component: &Path) -> Result<()> {
    if !artifact.exists() {
        bail!(
            "expected build artifact {} not found after build",
            artifact.display()
        );
    }
    std::fs::copy(artifact, component)
        .with_context(|| format!("copying {} → {}", artifact.display(), component.display()))?;
    eprintln!("  {} → {}", artifact.display(), component.display());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn deps(toml: &str) -> Vec<Dep> {
        #[derive(serde::Deserialize)]
        struct Deps {
            deps: Vec<Dep>,
        }
        toml::from_str::<Deps>(toml).unwrap().deps
    }

    /// A dep's flags resolve the deps listed before it (libcurl against
    /// mbedTLS), and refuse one listed after it, which isn't built yet.
    #[test]
    fn dep_flags_refer_to_earlier_deps() {
        let deps = deps(
            r#"
            [[deps]]
            name = "mbedtls"
            license = "Apache-2.0"
            dir = "scratch/mbedtls"

            [[deps]]
            name = "curl"
            license = "curl"
            dir = "scratch/curl"
            cflags = ["-I{dep:mbedtls:src}/include"]
            cmake_flags = ["-DMBEDTLS_LIBRARY={dep:mbedtls:build}/library/libmbedtls.a", "-DX={repo}"]

            [[deps]]
            name = "early"
            license = "MIT"
            dir = "scratch/early"
            cmake_flags = ["-DY={dep:later:build}"]
            "#,
        );
        let root = Path::new("/r");
        let (cflags, cmake_flags) = dep_flags(&deps[1], &deps[..1], root).unwrap();
        assert_eq!(cflags, ["-I/r/scratch/mbedtls/include"]);
        assert_eq!(
            cmake_flags,
            [
                "-DMBEDTLS_LIBRARY=/r/scratch/mbedtls/build/library/libmbedtls.a",
                "-DX=/r",
            ]
        );
        let err = dep_flags(&deps[2], &deps[..2], root).unwrap_err();
        assert!(err.to_string().contains("isn't built before it"), "{err}");
    }

    /// A C program's exit status survives, from `main`'s return and from
    /// `exit`, mod 256 as natively. Opt-in: it needs `WASI_SDK_PATH`, and
    /// `wit-bindgen` and `wasmtime` on PATH (`mise exec --` provides them).
    #[test]
    #[ignore = "needs WASI_SDK_PATH, wit-bindgen and wasmtime"]
    fn exit_statuses_survive() {
        let sdk = PathBuf::from(std::env::var_os("WASI_SDK_PATH").unwrap());
        let clang = sdk.join("bin/clang");
        let dir = std::env::temp_dir().join(format!("eryx-build-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("t.c"),
            "#include <stdlib.h>\n\
             int main(int argc, char **argv) {\n\
                 if (argc > 2) exit(atoi(argv[2]));\n\
                 return atoi(argv[1]);\n\
             }\n",
        )
        .unwrap();
        let manifest: Manifest = toml::from_str(
            r#"
            name = "t"
            license = "MIT"
            lang = "c"
            [source]
            repo = "https://example.com/t"
            ref = "0"
            dir = "."
            [build]
            sources = ["t.c"]
            [output]
            dir = "."
            "#,
        )
        .unwrap();
        let objs = exit_code_objs(&manifest, &clang, &dir).unwrap();
        let component = dir.join("t.wasm");
        build_single(&manifest, &clang, &dir, &component, &objs).unwrap();
        let cases: [(&[&str], i32); 7] = [
            (&["0"], 0),
            (&["3"], 3),
            (&["42"], 42),
            (&["0", "126"], 126),
            (&["0", "255"], 255),
            (&["256"], 0),
            (&["0", "257"], 1),
        ];
        for (args, want) in cases {
            let status = std::process::Command::new("wasmtime")
                .arg("run")
                .arg(&component)
                .args(args)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(want), "t {args:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
