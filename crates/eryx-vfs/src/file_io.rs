//! Host file I/O shared by descriptors and streams: offset-based reads and
//! writes, and append handles.
//!
//! Windows operations also update the OS file cursor. All callers supply an
//! explicit offset and keep their stream positions separately.

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        file.seek_read(buf, offset)
    }
}

/// Open a new handle to `file` for appending (`O_APPEND`; append-only access
/// on Windows), so every write lands at the end of the file as it is then,
/// even with other processes writing to it.
///
/// `file` must be open for writing; the new handle gets no more access and
/// is checked to be the same file. The reopen must not walk a path that
/// another sandbox sharing the mount could redirect (through symlinks or
/// renames) to somewhere outside it, or make it block on a FIFO.
pub(crate) fn reopen_for_append(file: &File) -> io::Result<File> {
    #[cfg(unix)]
    {
        unix_reopen::reopen_for_append(file)
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::OpenOptionsExt;
        // std's `append(true)` asks for `FILE_GENERIC_WRITE & !FILE_WRITE_DATA`,
        // which can't read attributes, and both the same-file check and the
        // per-write size check need them.
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;
        const FILE_WRITE_DATA: u32 = 0x0002;
        const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
        let mut options = cap_primitives::fs::OpenOptions::new();
        options
            .append(true)
            .access_mode((FILE_GENERIC_WRITE & !FILE_WRITE_DATA) | FILE_READ_ATTRIBUTES);
        // `ReOpenFile` on the handle itself: no path is involved.
        cap_primitives::fs::reopen(file, &options)
    }
}

#[cfg(unix)]
mod unix_reopen {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    use rustix::fs::OFlags;

    pub(super) fn reopen_for_append(file: &File) -> io::Result<File> {
        let access = rustix::fs::fcntl_getfl(file)? & OFlags::RWMODE;
        if access != OFlags::WRONLY && access != OFlags::RDWR {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file is not open for writing",
            ));
        }
        // Non-blocking so a FIFO can't stall the open; cleared afterwards.
        let flags = OFlags::NONBLOCK | OFlags::NOCTTY;
        let new = open_same(file, flags)?;
        let (old_meta, new_meta) = (file.metadata()?, new.metadata()?);
        if (old_meta.dev(), old_meta.ino()) != (new_meta.dev(), new_meta.ino()) {
            return Err(io::Error::other("reopened a different file"));
        }
        let flags = rustix::fs::fcntl_getfl(&new)?;
        rustix::fs::fcntl_setfl(&new, flags - OFlags::NONBLOCK)?;
        Ok(new)
    }

    pub(super) fn options(flags: OFlags) -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        // Open flags are small positive values, so this cast doesn't wrap.
        options.append(true).custom_flags(flags.bits() as i32);
        options
    }

    /// Linux: open the fd's `/proc/self/fd` magic link, which resolves
    /// straight to the open file's inode with no path walk (and works even
    /// if the file has been deleted).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn open_same(file: &File, flags: OFlags) -> io::Result<File> {
        use std::os::fd::AsRawFd;
        options(flags).open(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }

    /// Apple: open the fd's path. `NOFOLLOW` refuses a symlink in its last
    /// component, and the same-file check catches anything redirected
    /// earlier in the path.
    #[cfg(target_vendor = "apple")]
    fn open_same(file: &File, flags: OFlags) -> io::Result<File> {
        use std::os::unix::ffi::OsStrExt;
        let path = rustix::fs::getpath(file)?;
        options(flags | OFlags::NOFOLLOW).open(std::ffi::OsStr::from_bytes(path.to_bytes()))
    }

    /// Elsewhere there's no safe way to reopen an fd, so append fails closed.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    fn open_same(_file: &File, _flags: OFlags) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reopening a host file for append is not supported on this platform",
        ))
    }
}

pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        file.write_at(buf, offset)
    }
    #[cfg(windows)]
    {
        file.seek_write(buf, offset)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reopen_appends_to_the_same_file() {
        let named = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(named.path(), b"ab").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(named.path())
            .unwrap();
        let mut appender = reopen_for_append(&file).unwrap();
        io::Write::write_all(&mut appender, b"cd").unwrap();
        assert_eq!(std::fs::read(named.path()).unwrap(), b"abcd");
    }

    #[test]
    fn reopen_needs_a_writable_handle() {
        let named = tempfile::NamedTempFile::new().unwrap();
        let file = File::open(named.path()).unwrap();
        let err = reopen_for_append(&file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    /// The reopen follows the fd, not a path: it still works once the file's
    /// name is gone, and a different file at the old name isn't touched.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn reopen_does_not_resolve_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"other").unwrap();

        let mut appender = reopen_for_append(&file).unwrap();
        io::Write::write_all(&mut appender, b"x").unwrap();
        assert_eq!(file.metadata().unwrap().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"other");
    }

    /// The flags the reopen opens with can't block on a FIFO: with no reader
    /// the open fails straight away instead of waiting for one.
    #[cfg(unix)]
    #[test]
    fn reopen_flags_never_block_on_a_fifo() {
        use rustix::fs::OFlags;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        // rustix has no portable mkfifo (`mknodat` is missing on Apple).
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = unix_reopen::options(OFlags::NONBLOCK | OFlags::NOCTTY).open(&fifo);
            let _ = tx.send(result.map(drop).map_err(|e| e.raw_os_error()));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("opening the FIFO blocked");
        assert_eq!(result, Err(Some(rustix::io::Errno::NXIO.raw_os_error())));
    }
}
