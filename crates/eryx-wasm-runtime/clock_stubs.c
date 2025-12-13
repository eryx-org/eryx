// Stub definitions for CLOCK_*_CPUTIME_ID symbols
// These are needed by Rust's libc crate but not provided by WASI libc
// Values match the WASI definitions in time.h

int _CLOCK_PROCESS_CPUTIME_ID = 2;
int _CLOCK_THREAD_CPUTIME_ID = 3;
