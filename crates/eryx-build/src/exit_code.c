// Linked into every C-lane component: keep the program's exit status.
//
// wasi-libc on wasip2 reports a status only as `wasi:cli/exit.exit(result)`
// (from `_Exit`) or as `wasi:cli/run.run`'s result (when `main` returns),
// so every non-zero status reached the host as 1: awk's `exit 3`, git's
// 128, `curl -f`'s 22. This calls `exit-with-code` (WASI 0.2.12) instead.
//
// - `_Exit` replaces wasi-libc's (`exit` and `_exit` end in it), so an
//   explicit `exit(n)` passes on `n & 0xff`, as a native exit status does.
// - `__wrap___main_void` (linked with `--wrap=__main_void`) turns a
//   non-zero return from `main` into `exit(n)`, as native `_start` does.
//
// Compiled on its own by eryx-build, with exit_code.wit's component type.

#include <stdint.h>

__attribute__((import_module("wasi:cli/exit@0.2.12"), import_name("exit-with-code")))
extern void eryx_exit_with_code(int32_t status_code);

_Noreturn void exit(int status);

_Noreturn void _Exit(int status) {
    eryx_exit_with_code(status & 0xff);
    __builtin_unreachable();
}

int __real___main_void(void);

int __wrap___main_void(void) {
    int status = __real___main_void();
    if (status != 0) {
        exit(status);
    }
    return 0;
}
