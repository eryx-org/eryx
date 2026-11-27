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

# ---------------------------------------------------------------------------
# Show what `ls -l` can know about permissions on WASI.
#
# WASI's stat has no permission bits, so std's `readonly()` is always true
# and the non-unix display_permissions printed `r-xr-xr-x` for every file,
# even ones the run can write: agents read that as "can't write here". A
# file's writability on WASI comes from its mount, which `access(W_OK)`
# reports, so show the modes a file or directory would have on an ordinary
# filesystem: rw-r--r-- and rwxr-xr-x if writable, r--r--r-- and r-xr-xr-x
# if not, and devices as rw-rw-rw-. Idempotent.
# ---------------------------------------------------------------------------
display=src/uu/ls/src/display.rs
[ -f "$display" ] || { echo "patch-uucore-wasip2: $display not found (wrong dir?)" >&2; exit 1; }
if grep -q 'eryx: modes on WASI' "$fs"; then
    echo "patch-uucore-wasip2: wasi_permissions already patched"
else
    cat >> "$fs" <<'EOF'

// eryx: modes on WASI
#[cfg(target_os = "wasi")]
/// The permissions to show for the file at `path` on WASI, which has no file
/// modes: for a file or directory, those of a writable one if `access(W_OK)`
/// allows writing it, else those of a read-only one. Devices (/dev/null and
/// the like) are writable; a symlink keeps what it had.
pub fn wasi_permissions(metadata: &fs::Metadata, path: &Path) -> Option<&'static str> {
    use std::os::wasi::ffi::OsStrExt;
    use std::os::wasi::fs::FileTypeExt;
    let file_type = metadata.file_type();
    if file_type.is_char_device() || file_type.is_block_device() {
        return Some("rw-rw-rw-");
    }
    if !metadata.is_dir() && !metadata.is_file() {
        return None;
    }
    let writable = std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: a valid C string, which access only reads.
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0);
    Some(match (metadata.is_dir(), writable) {
        (true, true) => "rwxr-xr-x",
        (true, false) => "r-xr-xr-x",
        (false, true) => "rw-r--r--",
        (false, false) => "r--r--r--",
    })
}
EOF
    echo "patch-uucore-wasip2: wasi_permissions added to $fs"
fi
if grep -q 'eryx: modes on WASI' "$display"; then
    echo "patch-uucore-wasip2: ls modes already patched"
else
    perl -0pi -e 's/\Q            .extend(display_permissions(md, true).as_bytes());\E/            \/\/ eryx: modes on WASI\n            .extend({\n                let mut permissions = display_permissions(md, true);\n                #[cfg(target_os = "wasi")]\n                if let Some(wasi) = uucore::fs::wasi_permissions(md, item.path()) {\n                    permissions.replace_range(1.., wasi);\n                }\n                permissions\n            }.as_bytes());/' "$display"
    grep -q 'eryx: modes on WASI' "$display" || {
        echo "patch-uucore-wasip2: ERROR — failed to patch modes in $display" >&2
        exit 1
    }
    echo "patch-uucore-wasip2: ls shows modes from access(W_OK) on WASI in $display"
fi
