//! Offset-based host file I/O shared by descriptors and streams.
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
