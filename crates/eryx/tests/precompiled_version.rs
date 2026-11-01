//! A precompiled runtime records the eryx version that built it, and loading
//! it in any other version fails with [`eryx::Error::IncompatiblePrecompiled`]
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
    let path = dir.path().join("runtime.cwasm");
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
        matches!(err, eryx::Error::IncompatiblePrecompiled { .. }),
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
