//! A content-addressed cache of downloads, keyed by their integrity hash.

use std::path::{Path, PathBuf};

use crate::integrity::Integrity;
use crate::{Error, Result};

/// Downloads stored under their integrity hash, so a cached copy is only
/// ever used if it still hashes to what was asked for.
#[derive(Debug, Clone)]
pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    /// A store in `dir`, created on first write.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The cached bytes for `integrity`, if there are any that match it.
    /// A cached file that doesn't match (corrupted or tampered with) is
    /// removed and treated as missing.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but can't be read.
    pub fn get(&self, integrity: &Integrity) -> Result<Option<Vec<u8>>> {
        let path = self.path(integrity);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()))(e)),
        };
        if integrity.matches(&bytes) {
            Ok(Some(bytes))
        } else {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }

    /// Store `bytes`, which must match `integrity`. Writes go to a temporary
    /// file renamed into place, so readers never see a partial blob.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] if they don't match, or an I/O error.
    pub fn put(&self, what: &str, integrity: &Integrity, bytes: &[u8]) -> Result<()> {
        integrity.check(what, bytes)?;
        std::fs::create_dir_all(&self.dir)
            .map_err(Error::io(format!("creating {}", self.dir.display())))?;
        let tmp = tempfile::NamedTempFile::new_in(&self.dir)
            .map_err(Error::io(format!("writing to {}", self.dir.display())))?;
        std::fs::write(tmp.path(), bytes)
            .map_err(Error::io(format!("writing {}", tmp.path().display())))?;
        let path = self.path(integrity);
        tmp.persist(&path)
            .map_err(|e| Error::io(format!("writing {}", path.display()))(e.error))?;
        Ok(())
    }

    fn path(&self, integrity: &Integrity) -> PathBuf {
        self.dir.join(integrity.file_name())
    }

    /// The directory the blobs are in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn stores_checks_and_drops_tampered_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(dir.path().join("blobs"));
        let sri = Integrity::sha512_of(b"tarball");

        assert_eq!(blobs.get(&sri).unwrap(), None);
        assert!(blobs.put("x", &sri, b"other").is_err());
        blobs.put("x", &sri, b"tarball").unwrap();
        assert_eq!(blobs.get(&sri).unwrap().as_deref(), Some(&b"tarball"[..]));

        std::fs::write(blobs.path(&sri), b"tampered").unwrap();
        assert_eq!(blobs.get(&sri).unwrap(), None);
        assert!(!blobs.path(&sri).exists());
    }
}
