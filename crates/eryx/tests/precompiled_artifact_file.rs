//! Coverage for file-backed [`PrecompiledArtifact`]s.
//!
//! A file-backed artifact maps the component from the start of a file that
//! may carry trailing data (pyeryx factory files put their data-files archive
//! and footer after it), and must behave like the in-memory artifact for the
//! same bytes.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use eryx::PrecompiledArtifact;

fn shared() -> eryx::Engine {
    eryx::Engine::shared().unwrap()
}

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
    let uncached = unsafe {
        PrecompiledArtifact::from_file(&shared(), std::fs::File::open(&path).unwrap(), len)
    }
    .unwrap();
    #[allow(unsafe_code)]
    let cached = unsafe {
        PrecompiledArtifact::from_file_cached(&shared(), std::fs::File::open(&path).unwrap(), len)
    }
    .unwrap();

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
    let artifact = unsafe {
        PrecompiledArtifact::from_file(
            &shared(),
            std::fs::File::open(&path).unwrap(),
            component.len() as u64,
        )
    }
    .unwrap();

    let mut written = Vec::new();
    artifact.write_to(&mut written).unwrap();
    assert!(written == component, "write_to returned different bytes");
}

#[tokio::test]
async fn write_to_reads_the_mapped_file_after_the_path_is_replaced() {
    let (dir, path, component) = component_file(b"");

    #[allow(unsafe_code)]
    let artifact = unsafe {
        PrecompiledArtifact::from_file(
            &shared(),
            std::fs::File::open(&path).unwrap(),
            component.len() as u64,
        )
    }
    .unwrap();

    // Replace the path by renaming, as pyeryx's `save()` does. The artifact
    // keeps the file it was given, both for executing and for `write_to`.
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
    let result = unsafe {
        PrecompiledArtifact::from_file(
            &shared(),
            std::fs::File::open(&path).unwrap(),
            component.len() as u64 + 1,
        )
    };
    assert!(result.is_err());
}

#[tokio::test]
async fn from_file_maps_the_open_handle_not_the_path() {
    let (dir, path, component) = component_file(b"");
    let file = std::fs::File::open(&path).unwrap();

    // Swap the path between opening (and checking) the file and mapping it.
    let replacement = dir.path().join("replacement.bin");
    std::fs::write(&replacement, b"not a component").unwrap();
    std::fs::rename(&replacement, &path).unwrap();

    #[allow(unsafe_code)]
    let artifact =
        unsafe { PrecompiledArtifact::from_file(&shared(), file, component.len() as u64) }.unwrap();
    assert_eq!(run(&artifact).await, b"42\n");
}

#[tokio::test]
async fn precompiled_file_builder_maps_once_for_a_pool() {
    let (dir, path, _component) = component_file(b"");
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();
    #[allow(unsafe_code)]
    let builder = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_file(&path)
            .with_python_stdlib(resources.stdlib())
    };
    let config = eryx::PoolConfig {
        max_size: 2,
        min_idle: 0,
        ..eryx::PoolConfig::default()
    };
    let pool = eryx::SandboxPool::new(builder, config).await.unwrap();

    let first = pool.acquire().await.unwrap();
    assert_eq!(first.execute("print(6 * 7)").await.unwrap().stdout, b"42\n");

    // Replacing the file only affects new builders: the pool's next sandbox
    // reuses the component mapped for the first one.
    let replacement = dir.path().join("replacement.bin");
    std::fs::write(&replacement, b"not a component").unwrap();
    std::fs::rename(&replacement, &path).unwrap();

    let second = pool.acquire().await.unwrap();
    assert_eq!(
        second.execute("print(6 * 7)").await.unwrap().stdout,
        b"42\n"
    );
    assert_eq!(pool.stats().total_creations, 2);
}

#[tokio::test]
async fn from_file_with_hash_matches_the_computed_key() {
    let (_dir, path, component) = component_file(b"trailer");
    let len = component.len() as u64;

    let hash = PrecompiledArtifact::new(component.clone())
        .content_hash()
        .unwrap();
    #[allow(unsafe_code)]
    let mapped = unsafe {
        PrecompiledArtifact::from_file(&shared(), std::fs::File::open(&path).unwrap(), len)
    }
    .unwrap();
    assert_eq!(mapped.content_hash().unwrap(), hash);

    #[allow(unsafe_code)]
    let cached = unsafe {
        PrecompiledArtifact::from_file_cached(&shared(), std::fs::File::open(&path).unwrap(), len)
    }
    .unwrap();
    assert_eq!(cached.content_hash().unwrap(), hash);

    #[allow(unsafe_code)]
    let stored = unsafe {
        PrecompiledArtifact::from_file_with_hash(
            &shared(),
            std::fs::File::open(&path).unwrap(),
            len,
            hash,
        )
    }
    .unwrap();
    assert_eq!(stored.content_hash().unwrap(), hash);
    assert_eq!(run(&stored).await, b"42\n");
}

/// A file-backed artifact is bound to the engine it was mapped into, so a
/// sandbox on any other engine is refused rather than silently remapped.
#[tokio::test]
async fn file_backed_artifact_rejects_another_engine() {
    let (_dir, path, component) = component_file(b"");
    let other = eryx::Engine::new(
        eryx::EngineConfig::default()
            .with_allocator(eryx::InstanceAllocator::OnDemand)
            .with_warm_instances(0),
    )
    .unwrap();
    #[allow(unsafe_code)]
    let artifact = unsafe {
        PrecompiledArtifact::from_file(
            &other,
            std::fs::File::open(&path).unwrap(),
            component.len() as u64,
        )
    }
    .unwrap();
    let resources = eryx::embedded::EmbeddedResources::get().unwrap();

    #[allow(unsafe_code)]
    let on_shared = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_artifact(artifact.clone())
            .with_python_stdlib(resources.stdlib())
    }
    .build();
    let Err(err) = on_shared else {
        panic!("an artifact mapped into another engine must be refused");
    };
    assert!(err.to_string().contains("different engine"), "{err}");

    #[allow(unsafe_code)]
    let on_other = unsafe {
        eryx::Sandbox::builder()
            .with_precompiled_artifact(artifact)
            .with_python_stdlib(resources.stdlib())
    }
    .with_engine(other)
    .build()
    .unwrap();
    assert_eq!(
        on_other.execute("print(6 * 7)").await.unwrap().stdout,
        b"42\n"
    );
}
