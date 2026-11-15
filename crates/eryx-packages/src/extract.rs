//! Unpacking package archives without trusting them.

use std::cell::Cell;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use crate::{Error, Result};

/// How much one archive may unpack to.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    /// Total bytes of file contents.
    pub max_bytes: u64,
    /// Entries (files and directories).
    pub max_entries: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_entries: 50_000,
        }
    }
}

/// Tar bytes allowed per entry beyond its contents: its header block, padding,
/// and pax or GNU long-name headers.
const STREAM_BYTES_PER_ENTRY: u64 = 1536;

/// Unpack a gzipped tarball into `dest`, dropping each entry's first path
/// component (npm tarballs put everything under `package/`).
///
/// Only regular files and directories are unpacked, and only to paths inside
/// `dest`: symlinks, hard links, devices, absolute paths and `..` are refused,
/// as is anything over `limits`. The decompressed stream is capped too, so a
/// gzip bomb in a header that `tar` buffers (pax or GNU long names) can't
/// exhaust memory. Entries under a top-level `node_modules/` (bundled
/// dependencies) are skipped: an install's tree comes only from its
/// lockfile. Returns the bytes written.
///
/// # Errors
///
/// [`Error::Extract`] for a malformed or unsafe archive, [`Error::Limit`]
/// past the limits, or an I/O error writing `dest`.
pub fn unpack_tgz_stripped(
    what: &str,
    tgz: &[u8],
    dest: &Path,
    limits: ExtractLimits,
) -> Result<u64> {
    let bad = |message: String| Error::Extract {
        what: what.to_string(),
        message,
    };
    let max_stream = limits
        .max_bytes
        .saturating_add((limits.max_entries as u64).saturating_mul(STREAM_BYTES_PER_ENTRY));
    let exceeded = Rc::new(Cell::new(false));
    let stream = Capped {
        inner: flate2::read::GzDecoder::new(tgz),
        remaining: max_stream,
        exceeded: Rc::clone(&exceeded),
    };
    let over_stream = || {
        Error::Limit(format!(
            "{what} decompresses to more than {max_stream} bytes"
        ))
    };
    let bad = |message: String| {
        if exceeded.get() {
            over_stream()
        } else {
            bad(message)
        }
    };
    let mut archive = tar::Archive::new(stream);
    let mut written = 0u64;
    let entries = archive.entries().map_err(|e| bad(e.to_string()))?;
    for (index, entry) in entries.enumerate() {
        if index >= limits.max_entries {
            return Err(Error::Limit(format!(
                "{what} has more than {} entries",
                limits.max_entries
            )));
        }
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let raw = entry.path().map_err(|e| bad(e.to_string()))?.into_owned();
        let Some(relative) = strip_first(&raw) else {
            return Err(bad(format!("unsafe path {}", raw.display())));
        };
        if relative.as_os_str().is_empty()
            || relative.components().next() == Some(Component::Normal("node_modules".as_ref()))
        {
            continue;
        }
        let target = dest.join(&relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(Error::io(format!("creating {}", target.display())))?;
            continue;
        }
        if !kind.is_file() {
            return Err(bad(format!(
                "{} is not a regular file or directory",
                raw.display()
            )));
        }
        written = written
            .checked_add(entry.size())
            .filter(|w| *w <= limits.max_bytes)
            .ok_or_else(|| {
                Error::Limit(format!(
                    "{what} unpacks to more than {} bytes",
                    limits.max_bytes
                ))
            })?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::io(format!("creating {}", parent.display())))?;
        }
        let size = entry.size();
        let mut contents = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        entry
            .by_ref()
            .take(size)
            .read_to_end(&mut contents)
            .map_err(|e| bad(e.to_string()))?;
        std::fs::write(&target, contents)
            .map_err(Error::io(format!("writing {}", target.display())))?;
    }
    Ok(written)
}

/// A reader that fails once more than `remaining` bytes have been read,
/// noting it in `exceeded` (errors pass through `tar` as strings).
struct Capped<R> {
    inner: R,
    remaining: u64,
    exceeded: Rc<Cell<bool>>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        match self.remaining.checked_sub(n as u64) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(n)
            }
            None => {
                self.exceeded.set(true);
                Err(std::io::Error::other("the archive is over its size limit"))
            }
        }
    }
}

/// `path` without its first component, if every component is a plain name.
fn strip_first(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        Component::Normal(_) => {}
        _ => return None,
    }
    let mut out = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    /// A gzipped tarball of `files` (path, contents), each under `package/`
    /// unless the path already has a first component of its own.
    pub(crate) fn tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    /// A tarball with one raw entry, bypassing `tar`'s own path checks.
    fn raw_tgz(name: &[u8], kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_entry_type(kind);
        if let Some(link) = link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        let mut tar = header.as_bytes().to_vec();
        tar.extend_from_slice(&[0; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn unpacks_under_dest_without_the_top_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tgz(&[("package/package.json", b"{}"), ("package/lib/a.js", b"x")]);
        let n = unpack_tgz_stripped("t", &archive, dir.path(), ExtractLimits::default()).unwrap();
        assert_eq!(n, 3);
        assert_eq!(std::fs::read(dir.path().join("lib/a.js")).unwrap(), b"x");
        assert!(dir.path().join("package.json").exists());
    }

    #[test]
    fn refuses_unsafe_entries() {
        let dir = tempfile::tempdir().unwrap();
        for archive in [
            raw_tgz(b"package/../../escape.js", tar::EntryType::Regular, None),
            raw_tgz(b"/abs/x.js", tar::EntryType::Regular, None),
            raw_tgz(
                b"package/link",
                tar::EntryType::Symlink,
                Some("/etc/passwd"),
            ),
            raw_tgz(b"package/hard", tar::EntryType::Link, Some("package/x")),
            raw_tgz(b"package/dev", tar::EntryType::Char, None),
        ] {
            let err = unpack_tgz_stripped("t", &archive, dir.path(), ExtractLimits::default())
                .unwrap_err();
            assert!(matches!(err, Error::Extract { .. }), "{err}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn caps_headers_tar_buffers_before_the_entry_limits_apply() {
        // A 64 MiB pax header compresses to a few KiB, and `tar` reads all
        // of it into memory before yielding the entry it describes.
        let payload = vec![b'a'; 64 * 1024 * 1024];
        let mut header = tar::Header::new_ustar();
        header.set_path("package/PaxHeader").unwrap();
        header.set_size(payload.len() as u64);
        header.set_entry_type(tar::EntryType::XHeader);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, payload.as_slice()).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let archive = gz.finish().unwrap();
        assert!(archive.len() < 1024 * 1024);

        let dir = tempfile::tempdir().unwrap();
        let limits = ExtractLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        };
        let err = unpack_tgz_stripped("t", &archive, dir.path(), limits).unwrap_err();
        assert!(matches!(err, Error::Limit(_)), "{err}");
    }

    #[test]
    fn skips_bundled_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tgz(&[
            ("package/index.js", b"x"),
            ("package/node_modules/dep/index.js", b"bundled"),
            ("package/lib/node_modules/kept.js", b"y"),
        ]);
        unpack_tgz_stripped("t", &archive, dir.path(), ExtractLimits::default()).unwrap();
        assert!(dir.path().join("index.js").exists());
        assert!(!dir.path().join("node_modules").exists());
        assert!(dir.path().join("lib/node_modules/kept.js").exists());
    }

    #[test]
    fn enforces_limits() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tgz(&[("package/a", b"12345"), ("package/b", b"67890")]);
        let small = ExtractLimits {
            max_bytes: 6,
            max_entries: 10,
        };
        assert!(matches!(
            unpack_tgz_stripped("t", &archive, dir.path(), small),
            Err(Error::Limit(_))
        ));
        let few = ExtractLimits {
            max_bytes: 100,
            max_entries: 1,
        };
        assert!(matches!(
            unpack_tgz_stripped("t", &archive, dir.path(), few),
            Err(Error::Limit(_))
        ));
    }
}
