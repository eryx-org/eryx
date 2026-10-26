//! Checks a runtime cached by `eryx-precompile setup` before `build.rs`
//! embeds it.
//!
//! This lives in its own file so `tests/cached_runtime_check.rs` can test it;
//! build scripts have no test harness of their own.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Path of the checksum file `setup` writes next to a cached runtime.
pub fn checksum_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

/// Read the cached runtime at `path` and return its bytes if it can be used.
///
/// The runtime must match the SHA-256 in its checksum file. On Unix, the
/// runtime, the checksum file and the directory holding them must also be
/// owned by the current user and not be group- or world-writable, since the
/// runtime is native code that ends up in the binary being built.
///
/// # Errors
///
/// Returns why the runtime can't be used.
pub fn read_verified(path: &Path) -> Result<Vec<u8>, String> {
    let dir = path.parent().ok_or("path has no parent directory")?;
    let meta =
        fs::metadata(dir).map_err(|e| format!("could not inspect {}: {e}", dir.display()))?;
    if !is_private(&meta) {
        return Err(format!(
            "{} is owned by another user or writable by other users",
            dir.display()
        ));
    }

    let checksum = checksum_path(path);
    for file in [path, checksum.as_path()] {
        let meta = fs::symlink_metadata(file)
            .map_err(|e| format!("could not inspect {}: {e}", file.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", file.display()));
        }
        if !is_private(&meta) {
            return Err(format!(
                "{} is owned by another user or writable by other users",
                file.display()
            ));
        }
    }

    let expected = fs::read_to_string(&checksum)
        .map_err(|e| format!("could not read {}: {e}", checksum.display()))?;
    // The checksum file is `sha256sum` output: `<hex>  <file name>`.
    let expected = expected.split_whitespace().next().unwrap_or_default();
    let bytes = fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if !hex::encode(Sha256::digest(&bytes)).eq_ignore_ascii_case(expected) {
        return Err(format!(
            "{} doesn't match the checksum in {}",
            path.display(),
            checksum.display()
        ));
    }
    Ok(bytes)
}

/// Whether a file or directory is owned by the current user and not group- or
/// world-writable. Always true on platforms without Unix permissions.
///
/// Mirrors `eryx::cache::is_private`, which a build script can't call.
fn is_private(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == rustix::process::geteuid().as_raw() && meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}
