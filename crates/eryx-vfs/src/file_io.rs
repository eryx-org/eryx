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

/// Open a new handle to `file` for appending (`O_APPEND`; `FILE_APPEND_DATA`
/// on Windows), so every write lands at the end of the file as it is then,
/// even with other processes writing to it.
///
/// `file` must be open for writing. The new handle is checked to be the same
/// file and gets no more access than `file` has.
pub(crate) fn reopen_for_append(file: &File) -> io::Result<File> {
    let mut options = cap_primitives::fs::OpenOptions::new();
    options.append(true);
    cap_primitives::fs::reopen(file, &options)
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
