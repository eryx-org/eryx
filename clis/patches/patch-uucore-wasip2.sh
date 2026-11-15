#!/usr/bin/env bash
# Patch uutils/coreutils for the wasm32-wasip2 Rust lane. Run from the
# uutils source dir by the eryx-build Rust lane (manifest `vendor_patch`).
# Idempotent.
set -euo pipefail

# ---------------------------------------------------------------------------
# 1) Unstable `wasip2` library feature.
#
# uucore uses std::os::wasi::ffi (OsStrExt/OsStringExt), which on the
# wasm32-wasip2 target is gated behind the unstable `wasip2` library feature.
# Add the feature attribute to uucore's crate root; the build sets
# RUSTC_BOOTSTRAP=1 (manifest [build] env) so the pinned stable toolchain
# accepts it. (One attribute covers uucore's lib + its fs/display modules.)
# ---------------------------------------------------------------------------
f=src/uucore/src/lib/lib.rs
[ -f "$f" ] || { echo "patch-uucore-wasip2: $f not found (wrong dir?)" >&2; exit 1; }

if grep -q 'feature(wasip2)' "$f"; then
    echo "patch-uucore-wasip2: wasip2 feature already patched"
else
    sed -i '1i #![cfg_attr(target_arch = "wasm32", feature(wasip2))]' "$f"
    grep -q 'feature(wasip2)' "$f" || {
        echo "patch-uucore-wasip2: ERROR — failed to add feature attr to $f" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: added #![feature(wasip2)] to $f"
fi

# ---------------------------------------------------------------------------
# 2) Flush stdout/stderr before the multicall exits.
#
# Rust's `io::stdout()` is line-buffered (LineWriter): a final line with no
# trailing newline stays buffered. The multicall dispatches a util then calls
# `process::exit(code)` immediately (src/bin/coreutils.rs), which skips the
# atexit flush — so when the host spawns e.g. `cat` on data not ending in '\n',
# the last partial line never reaches the WASI stdout stream and is lost. On
# native this is masked because exit flushes libc buffers. Force an explicit
# flush of both streams before exiting the util. `io`/`Write` are already in
# scope in that file. Idempotent.
# ---------------------------------------------------------------------------
m=src/bin/coreutils.rs
[ -f "$m" ] || { echo "patch-uucore-wasip2: $m not found (wrong dir?)" >&2; exit 1; }

if grep -q 'eryx: flush before exit' "$m"; then
    echo "patch-uucore-wasip2: multicall flush already patched"
else
    perl -0pi -e 's/\Q                process::exit(uumain(vec![util_os].into_iter().chain(args)));\E/                let code = uumain(vec![util_os].into_iter().chain(args));\n                let _ = io::stdout().flush(); \/\/ eryx: flush before exit\n                let _ = io::stderr().flush();\n                process::exit(code);/' "$m"
    grep -q 'eryx: flush before exit' "$m" || {
        echo "patch-uucore-wasip2: ERROR — failed to add multicall flush to $m" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: added stdout/stderr flush before multicall exit in $m"
fi
