#!/usr/bin/env bash
# Patch uutils/coreutils for the wasm32-wasip2 Rust lane. Run from the
# uutils source dir by the eryx-build Rust lane (manifest `vendor_patch`).
# Idempotent.
set -euo pipefail

# ---------------------------------------------------------------------------
# Treat character and block devices as devices on WASI.
#
# The shell's /dev/null, /dev/zero and /dev/urandom report the WASI
# `character-device` type (#832), but the utils only check for devices under
# cfg(unix), and std's FileTypeExt for WASI is behind `wasi_ext`. So `cat
# /dev/null` failed with "unknown filetype", `cp /dev/null f` with std's
# "neither a regular file nor a symlink" (fs::copy only takes regular files),
# and `ls -l /dev` showed `-` where it should show `c`. Read devices like
# files in cat, stream their contents in cp, and show their type in the
# non-unix display_permissions that ls uses. Idempotent.
# ---------------------------------------------------------------------------
fs=src/uucore/src/lib/features/fs.rs
lib=src/uucore/src/lib/lib.rs
for f in "$fs" "$lib"; do
    [ -f "$f" ] || { echo "patch-uucore-wasip2: $f not found (wrong dir?)" >&2; exit 1; }
done

cat=src/uu/cat/src/cat.rs
[ -f "$cat" ] || { echo "patch-uucore-wasip2: $cat not found (wrong dir?)" >&2; exit 1; }
if grep -q 'eryx: devices on WASI' "$cat"; then
    echo "patch-uucore-wasip2: cat devices already patched"
else
    sed -i '1i #![cfg_attr(target_os = "wasi", feature(wasip2, wasi_ext))]' "$cat"
    perl -0pi -e 's/(    match ft \{\n)(        #\[cfg\(unix\)\]\n        ft if ft\.is_block_device\(\))/$1        \/\/ eryx: devices on WASI\n        #[cfg(target_os = "wasi")]\n        ft if std::os::wasi::fs::FileTypeExt::is_char_device(&ft)\n            || std::os::wasi::fs::FileTypeExt::is_block_device(&ft) =>\n        {\n            Ok(InputType::File)\n        }\n$2/' "$cat"
    grep -q 'eryx: devices on WASI' "$cat" || {
        echo "patch-uucore-wasip2: ERROR — failed to patch devices in $cat" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: cat reads devices on WASI in $cat"
fi

other=src/uu/cp/src/platform/other.rs
[ -f "$other" ] || { echo "patch-uucore-wasip2: $other not found (wrong dir?)" >&2; exit 1; }
if grep -q 'eryx: devices on WASI' "$other"; then
    echo "patch-uucore-wasip2: cp devices already patched"
else
    perl -0pi -e 's/\Q    fs::copy(source, dest).map_err(\E/    \/\/ eryx: devices on WASI\n    let copy = || -> std::io::Result<u64> {\n        if fs::metadata(source)?.is_file() {\n            fs::copy(source, dest)\n        } else {\n            std::io::copy(&mut fs::File::open(source)?, &mut fs::File::create(dest)?)\n        }\n    };\n    copy().map_err(/' "$other"
    grep -q 'eryx: devices on WASI' "$other" || {
        echo "patch-uucore-wasip2: ERROR — failed to patch devices in $other" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: cp streams devices on WASI in $other"
fi

# uucore's crate root needs the features for FileTypeExt in fs.rs.
if ! grep -q 'feature(wasip2, wasi_ext)' "$lib"; then
    sed -i '1i #![cfg_attr(target_os = "wasi", feature(wasip2, wasi_ext))]' "$lib"
fi
if grep -q 'eryx: devices on WASI' "$fs"; then
    echo "patch-uucore-wasip2: display_permissions devices already patched"
else
    perl -0pi -e 's/(        let file_type = if metadata\.is_symlink\(\) \{\n            .l.\n        \} else if metadata\.is_dir\(\) \{\n            .d.\n)(        \} else \{\n            .-.\n        \};)/$1        \/\/ eryx: devices on WASI\n        } else if std::os::wasi::fs::FileTypeExt::is_char_device(&metadata.file_type()) {\n            \x27c\x27\n        } else if std::os::wasi::fs::FileTypeExt::is_block_device(&metadata.file_type()) {\n            \x27b\x27\n$2/' "$fs"
    grep -q 'eryx: devices on WASI' "$fs" || {
        echo "patch-uucore-wasip2: ERROR — failed to patch display_permissions in $fs" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: display_permissions shows devices on WASI in $fs"
fi
