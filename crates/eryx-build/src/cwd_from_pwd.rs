//! Linked into every C and Rust component eryx-build makes (#978): start the
//! program in `$PWD`.
//!
//! WASI has no working directory. wasi-libc's emulated one starts at `/`
//! whatever the host says (it neither reads `PWD` nor calls
//! `wasi:cli/environment.initial-cwd`), so after the shell's `cd`, a child's
//! relative paths would resolve against `/`. eryx passes each child its
//! caller's directory in `PWD`; this constructor, run by `__wasm_call_ctors`
//! before `main`, moves there. Best effort: only an absolute `PWD`, and a
//! failed `chdir` leaves the program in `/`.
//!
//! Compiled on its own (`rustc --emit obj`) by eryx-build, not as part of
//! this crate.
#![no_std]

unsafe extern "C" {
    fn getenv(name: *const u8) -> *const u8;
    fn chdir(path: *const u8) -> i32;
    fn __errno_location() -> *mut i32;
}

extern "C" fn cwd_from_pwd() {
    // SAFETY: wasi-libc's getenv and chdir, on a NUL-terminated name and the
    // NUL-terminated value getenv returns, and its errno, which a failed
    // chdir mustn't leave set for the program to see.
    unsafe {
        let pwd = getenv(c"PWD".as_ptr().cast());
        if !pwd.is_null() && *pwd == b'/' {
            let errno = __errno_location();
            let saved = *errno;
            let _ = chdir(pwd);
            *errno = saved;
        }
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static CWD_FROM_PWD: extern "C" fn() = cwd_from_pwd;
