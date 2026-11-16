//! A precompiled runtime records the eryx version that built it, and loading
//! it in any other version fails with [`eryx::ErrorKind::IncompatiblePrecompiled`]
//! rather than a wasmtime or type-checking error.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

/// The version string this eryx stores in precompiled components, built
/// independently of eryx's own code.
fn module_version() -> String {
    format!(
        "eryx {} (wasmtime {})",
        env!("CARGO_PKG_VERSION"),
        wasmtime::ModuleVersionStrategy::default().as_str()
    )
}

/// Write the embedded runtime to a file with its stored version string
/// replaced by `forged`.
///
/// wasmtime stores the string as a length byte followed by the bytes, so the
/// replacement is padded to the original length to keep the rest of the
/// file in place. The version check fails before anything reads the padding.
fn forged_runtime(forged: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    let mut bytes = std::fs::read(resources.runtime()).unwrap();

    let version = module_version();
    let mut needle = vec![u8::try_from(version.len()).unwrap()];
    needle.extend_from_slice(version.as_bytes());
    let offsets: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle.as_slice())
        .map(|(offset, _)| offset)
        .collect();
    assert_eq!(
        offsets.len(),
        1,
        "the runtime should store `{version}` exactly once"
    );
    assert!(forged.len() <= version.len());

    let mut replacement = vec![u8::try_from(forged.len()).unwrap()];
    replacement.extend_from_slice(forged.as_bytes());
    replacement.resize(needle.len(), 0);
    bytes[offsets[0]..offsets[0] + needle.len()].copy_from_slice(&replacement);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime-python.cwasm");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn load(path: &std::path::Path) -> eryx::Error {
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    #[allow(unsafe_code)]
    let builder = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_file(path)
            .with_python_stdlib(resources.stdlib())
    };
    match builder.build() {
        Ok(_) => panic!("a forged runtime should not load"),
        Err(err) => err,
    }
}

#[test]
fn runtime_from_another_eryx_version_is_rejected() {
    let version = module_version();
    // Same length, different eryx version.
    let other = version
        .replacen("eryx ", "eryx 9", 1)
        .replacen(" (", "(", 1);
    let (_dir, path) = forged_runtime(&other);

    let err = load(&path);
    assert!(
        matches!(err.kind(), eryx::ErrorKind::IncompatiblePrecompiled { .. }),
        "unexpected error: {err:?}"
    );
    let message = err.to_string();
    assert!(message.contains(&format!("built by {other}")), "{message}");
    assert!(message.contains(&format!("this is {version}")), "{message}");
    assert!(
        message.contains(&format!(
            "rebuild it with eryx-precompile {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{message}"
    );
}

#[test]
fn runtime_from_before_eryx_versions_were_stored_is_rejected() {
    // Releases before 0.13.0 stored wasmtime's major version alone.
    let old = wasmtime::ModuleVersionStrategy::default()
        .as_str()
        .to_owned();
    let (_dir, path) = forged_runtime(&old);

    let message = load(&path).to_string();
    assert!(
        message.contains(&format!(
            "built by an eryx release before 0.13.0 (wasmtime {old})"
        )),
        "{message}"
    );
}

/// A precompiled command from this eryx version built with a wasm feature
/// this engine doesn't enable (as from a development build with other engine
/// settings) passes the version check, then fails wasmtime's feature check:
/// that's `IncompatiblePrecompiled` too, so it gets rebuilt like any other.
#[cfg(target_arch = "x86_64")]
#[test]
fn command_built_with_other_engine_features_is_rejected() {
    // eryx's engine settings, target-pinned as `CpuFeatureLevel::Baseline`
    // builds are (see tests/cpu_feature_portability.rs), plus a feature eryx
    // doesn't enable.
    let mut config = wasmtime::Config::new();
    config
        .module_version(wasmtime::ModuleVersionStrategy::Custom(module_version()))
        .unwrap();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.epoch_interruption(true);
    config.consume_fuel(true);
    config.memory_init_cow(true);
    config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
    config.async_stack_size(1024 * 1024);
    config
        .target(&target_lexicon::Triple::host().to_string())
        .unwrap();
    config.wasm_custom_page_sizes(true);
    let precompiled = wasmtime::Engine::new(&config)
        .unwrap()
        .precompile_component(&wat::parse_str("(component)").unwrap())
        .unwrap();

    #[allow(unsafe_code)]
    // SAFETY: built just above by wasmtime.
    let err = unsafe { eryx::Command::from_precompiled(&precompiled) }.unwrap_err();
    assert!(
        matches!(err.kind(), eryx::ErrorKind::IncompatiblePrecompiled { .. }),
        "unexpected error: {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains(
            "with other engine settings (it uses WebAssembly feature `custom_page_sizes`)"
        ),
        "{message}"
    );
}
