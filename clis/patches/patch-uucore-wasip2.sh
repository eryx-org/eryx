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

if grep -q 'feature(wasip2, wasi_ext)' "$f"; then
    echo "patch-uucore-wasip2: wasip2 features already patched"
else
    # `wasi_ext` is for std::os::wasi::fs::MetadataExt (patch 3).
    sed -i '/feature(wasip2)\]/d' "$f"
    sed -i '1i #![cfg_attr(target_arch = "wasm32", feature(wasip2, wasi_ext))]' "$f"
    grep -q 'feature(wasip2, wasi_ext)' "$f" || {
        echo "patch-uucore-wasip2: ERROR — failed to add feature attr to $f" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: added #![feature(wasip2, wasi_ext)] to $f"
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

# ---------------------------------------------------------------------------
# 3) Identify files by device and inode on WASI.
#
# uucore's `FileInformation` (used for "same file" and "already visited"
# checks: `ls -R`, `cp -r`, `mv`, `du`, `ln`) compares only the file type and
# size on WASI, so every empty directory looks like every other and `ls -R`
# stops descending (#972). std::os::wasi::fs::MetadataExt has the real `dev`
# and `ino` (wasi-libc derives them from the host's metadata-hash), as newer
# uucore uses. Replace the heuristic's comparison and hash with those.
# Idempotent.
# ---------------------------------------------------------------------------
fs=src/uucore/src/lib/features/fs.rs
[ -f "$fs" ] || { echo "patch-uucore-wasip2: $fs not found (wrong dir?)" >&2; exit 1; }

if grep -q 'eryx: dev and ino on WASI' "$fs"; then
    echo "patch-uucore-wasip2: FileInformation identity already patched"
else
    perl -0pi -e 's/\Q        self.0.file_type() == other.0.file_type() && self.0.len() == other.0.len()\E/        \/\/ eryx: dev and ino on WASI\n        use std::os::wasi::fs::MetadataExt;\n        self.0.dev() == other.0.dev() && self.0.ino() == other.0.ino()/' "$fs"
    perl -0pi -e 's/ {12}self\.0\.len\(\)\.hash\(state\);\n {12}self\.0\.file_type\(\)\.is_dir\(\)\.hash\(state\);/            use std::os::wasi::fs::MetadataExt;\n            self.0.dev().hash(state);\n            self.0.ino().hash(state);/' "$fs"
    if ! grep -q 'eryx: dev and ino on WASI' "$fs" || ! grep -q 'self.0.ino().hash(state)' "$fs"; then
        echo "patch-uucore-wasip2: ERROR — failed to patch FileInformation in $fs" >&2
        exit 1
    fi
    echo "patch-uucore-wasip2: FileInformation uses dev and ino on WASI in $fs"
fi

# ---------------------------------------------------------------------------
# 4) Don't copy file modes in `cp` on WASI.
#
# WASI has no file modes: std's `fs::set_permissions` calls wasi-libc's
# `chmod`, which fails with ENOSYS. `cp` copies the mode of every directory it
# creates, and of every file with `-p`/`--preserve=mode`, so `cp -r` printed
# "Function not implemented" per directory and `cp -p` exited 1 (#982). Skip
# the call there, as GNU cp skips modes a filesystem can't store. Idempotent.
# ---------------------------------------------------------------------------
cp=src/uu/cp/src/cp.rs
[ -f "$cp" ] || { echo "patch-uucore-wasip2: $cp not found (wrong dir?)" >&2; exit 1; }

if grep -q 'eryx: no file modes on WASI' "$cp"; then
    echo "patch-uucore-wasip2: cp mode preservation already patched"
else
    perl -0pi -e 's/\Q            fs::set_permissions(dest, source_perms)\E\n/            \/\/ eryx: no file modes on WASI\n            #[cfg(target_os = "wasi")]\n            let _ = source_perms;\n            #[cfg(not(target_os = "wasi"))]\n            fs::set_permissions(dest, source_perms)\n/' "$cp"
    if ! grep -q 'eryx: no file modes on WASI' "$cp"; then
        echo "patch-uucore-wasip2: ERROR — failed to patch mode preservation in $cp" >&2
        exit 1
    fi
    echo "patch-uucore-wasip2: cp skips mode preservation on WASI in $cp"
fi

# ---------------------------------------------------------------------------
# 5) Read timestamps from std's metadata in `cp` and `touch`.
#
# filetime's WASI backend leaves `FileTime::from_last_access_time` and
# `from_last_modification_time` as `unimplemented!()`, so `cp -p` (preserving
# timestamps) and `touch -r` panicked (#982). std's `Metadata::accessed` and
# `modified` work on WASI; read the times from those instead. This tree only
# builds for wasip2, so the change isn't gated. Idempotent.
# ---------------------------------------------------------------------------
for f in src/uu/cp/src/cp.rs src/uu/touch/src/touch.rs; do
    [ -f "$f" ] || { echo "patch-uucore-wasip2: $f not found (wrong dir?)" >&2; exit 1; }
    if ! grep -q 'FileTime::from_last_\(access\|modification\)_time(&[a-z_]*metadata)' "$f"; then
        echo "patch-uucore-wasip2: $f timestamps already patched"
        continue
    fi
    perl -pi -e 's/FileTime::from_last_access_time\(&(\w*metadata)\)/FileTime::from_system_time($1.accessed()?)/g; s/FileTime::from_last_modification_time\(&(\w*metadata)\)/FileTime::from_system_time($1.modified()?)/g' "$f"
    if grep -q 'FileTime::from_last_\(access\|modification\)_time(&[a-z_]*metadata)' "$f"; then
        echo "patch-uucore-wasip2: ERROR — failed to patch timestamps in $f" >&2
        exit 1
    fi
    echo "patch-uucore-wasip2: $f reads timestamps from std metadata"
done
