//! Linked into every Rust component eryx-build makes, with
//! `--wrap=__main_void` (#1131): keep the `ExitCode` `main` returns.
//!
//! When `main` returns, wasi-libc's `_start` reports its status as
//! `wasi:cli/run.run`'s result, which says only success or failure, so
//! `ExitCode::from(3)` reached the host as 1. `std::process::exit(n)` was
//! fine: it calls libc's `exit`, which Rust's wasi-libc ends with
//! `exit-with-code`. This sends a non-zero return from `main` down the same
//! path, as native `_start` does.
//!
//! Compiled on its own (`rustc --emit obj`) by eryx-build, not as part of
//! this crate.
#![no_std]

unsafe extern "C" {
    fn __real___main_void() -> i32;
    fn exit(status: i32) -> !;
}

#[unsafe(no_mangle)]
extern "C" fn __wrap___main_void() -> i32 {
    // SAFETY: the real `__main_void` (Rust's `main`) and wasi-libc's `exit`,
    // called once from `_start` as `__main_void` would be.
    let status = unsafe { __real___main_void() };
    if status != 0 {
        unsafe { exit(status) }
    }
    0
}
