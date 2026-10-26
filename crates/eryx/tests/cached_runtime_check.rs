//! Tests for the checks `build.rs` runs on a runtime cached by
//! `eryx-precompile setup` before embedding it.

#[path = "../build/cached_runtime.rs"]
mod cached_runtime;

use std::fs;
use std::path::{Path, PathBuf};

use cached_runtime::{checksum_path, read_verified};
use sha2::{Digest, Sha256};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Write a runtime and its checksum file the way `setup` does.
fn write_cached(dir: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let path = dir.join("runtime-v0.0.0-x86_64-linux.cwasm");
    fs::write(&path, bytes)?;
    let hash = hex::encode(Sha256::digest(bytes));
    fs::write(
        checksum_path(&path),
        format!("{hash}  runtime-v0.0.0-x86_64-linux.cwasm\n"),
    )?;
    #[cfg(unix)]
    for file in [path.clone(), checksum_path(&path)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

#[test]
fn cached_runtime_must_match_its_checksum() -> Result {
    let dir = tempfile::tempdir()?;
    let path = write_cached(dir.path(), b"compiled runtime")?;
    assert_eq!(read_verified(&path)?, b"compiled runtime");

    fs::write(&path, b"compiled")?;
    assert!(read_verified(&path).is_err());

    let path = write_cached(dir.path(), b"compiled runtime")?;
    fs::remove_file(checksum_path(&path))?;
    assert!(read_verified(&path).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn cached_runtime_must_not_be_writable_by_others() -> Result {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let path = write_cached(dir.path(), b"compiled runtime")?;

    fs::set_permissions(&path, fs::Permissions::from_mode(0o620))?;
    assert!(read_verified(&path).is_err());

    let path = write_cached(dir.path(), b"compiled runtime")?;
    fs::set_permissions(checksum_path(&path), fs::Permissions::from_mode(0o602))?;
    assert!(read_verified(&path).is_err());

    let path = write_cached(dir.path(), b"compiled runtime")?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777))?;
    assert!(read_verified(&path).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn cached_runtime_must_not_be_a_symlink() -> Result {
    let dir = tempfile::tempdir()?;
    let target = write_cached(dir.path(), b"compiled runtime")?;
    let link = dir.path().join("link.cwasm");
    std::os::unix::fs::symlink(&target, &link)?;
    fs::copy(checksum_path(&target), checksum_path(&link))?;
    assert!(read_verified(&link).is_err());
    Ok(())
}
