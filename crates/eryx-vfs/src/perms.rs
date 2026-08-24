//! Permission bits for VFS descriptors.
//!
//! These were re-exported from `wasmtime_wasi` up to wasmtime 47. wasmtime 48
//! collapsed them into a coarse `FsPerms { ReadOnly, ReadWrite }`, which cannot
//! express the distinctions the VFS host actually enforces — a directory that
//! is readable but not mutable, or a file opened write-only. They are vendored
//! here so the eryx public API and the host's permission checks are decoupled
//! from wasmtime's own preopen configuration type, which the VFS shadows
//! anyway. `FsPerms` is used only at the `preopened_dir` boundary.
//!
//! The flag values are unchanged from wasmtime-wasi 47, so serialized or
//! bit-compared values carry over.

bitflags::bitflags! {
    /// Permission bits for operating on a file.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct FilePerms: usize {
        /// This file can be read from.
        const READ = 0b1;

        /// This file can be written to.
        const WRITE = 0b10;
    }
}

bitflags::bitflags! {
    /// Permission bits for operating on a directory.
    ///
    /// Directories can be limited to being readonly. This will restrict what
    /// can be done with them, for example preventing creation of new files.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct DirPerms: usize {
        /// This directory can be read, for example its entries can be iterated
        /// over and files can be opened.
        const READ = 0b1;

        /// This directory can be mutated, for example by creating new files
        /// within it.
        const MUTATE = 0b10;
    }
}

impl DirPerms {
    /// The [`wasmtime_wasi::FsPerms`] that most closely covers these bits.
    ///
    /// wasmtime's preopen type only distinguishes readonly from read-write, so
    /// [`Self::MUTATE`] widens the preopen to `ReadWrite` and the finer-grained
    /// bits continue to be enforced by the VFS host on each operation.
    pub fn to_fs_perms(self) -> wasmtime_wasi::FsPerms {
        if self.contains(Self::MUTATE) {
            wasmtime_wasi::FsPerms::ReadWrite
        } else {
            wasmtime_wasi::FsPerms::ReadOnly
        }
    }
}
