//! Coverage for file-backed [`PrecompiledArtifact`]s.
//!
//! A file-backed artifact maps the component from the start of a file that
//! may carry trailing data (pyeryx factory files put their data-files archive
//! and footer after it), and must behave like the in-memory artifact for the
//! same bytes.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use eryx::PrecompiledArtifact;

/// Write the embedded runtime's component followed by `trailer` to a new file.
fn component_file(trailer: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    let component = std::fs::read(resources.runtime()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factory.bin");
    std::fs::write(&path, [component.as_slice(), trailer].concat()).unwrap();
    (dir, path, component)
}

async fn run(artifact: &PrecompiledArtifact) -> Vec<u8> {
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    #[allow(unsafe_code)]
    let sandbox = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_artifact(artifact.clone())
            .with_python_stdlib(resources.stdlib())
    }
    .build()
    .unwrap();
    sandbox.execute("print(6 * 7)").await.unwrap().stdout
}

#[tokio::test]
async fn file_backed_artifact_ignores_trailing_data() {
    let (_dir, path, component) = component_file(b"data files archive and footer");
    let len = component.len() as u64;

    #[allow(unsafe_code)]
    let uncached = unsafe { PrecompiledArtifact::from_file(&path, len) }.unwrap();
    #[allow(unsafe_code)]
    let cached = unsafe { PrecompiledArtifact::from_file_cached(&path, len) }.unwrap();

    for artifact in [&uncached, &cached] {
        assert_eq!(artifact.len(), component.len());
        assert_eq!(run(artifact).await, b"42\n");
        // A second executor reuses the mapped component.
        assert_eq!(run(artifact).await, b"42\n");
    }
}

#[tokio::test]
async fn write_to_returns_only_the_component() {
    let (_dir, path, component) = component_file(b"trailer");

    #[allow(unsafe_code)]
    let artifact =
        unsafe { PrecompiledArtifact::from_file(&path, component.len() as u64) }.unwrap();

    let mut written = Vec::new();
    artifact.write_to(&mut written).unwrap();
    assert!(written == component, "write_to returned different bytes");
}

// Windows refuses to rename over a file that is mapped.
#[cfg(not(windows))]
#[tokio::test]
async fn write_to_reads_the_mapped_file_after_the_path_is_replaced() {
    let (dir, path, component) = component_file(b"");

    #[allow(unsafe_code)]
    let artifact =
        unsafe { PrecompiledArtifact::from_file(&path, component.len() as u64) }.unwrap();

    // Replace the path by renaming, as pyeryx's `save()` does.
    let replacement = dir.path().join("replacement.bin");
    std::fs::write(&replacement, b"a different factory").unwrap();
    std::fs::rename(&replacement, &path).unwrap();

    let mut written = Vec::new();
    artifact.write_to(&mut written).unwrap();
    assert!(written == component, "write_to followed the replaced path");
    assert_eq!(run(&artifact).await, b"42\n");
}

#[tokio::test]
async fn from_file_rejects_a_file_shorter_than_the_component() {
    let (_dir, path, component) = component_file(b"");

    #[allow(unsafe_code)]
    let result = unsafe { PrecompiledArtifact::from_file(&path, component.len() as u64 + 1) };
    assert!(result.is_err());
}
