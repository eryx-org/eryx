#!/usr/bin/env bash
# Patch the curl source tree for the wasip2 C lane. Run from the curl
# source dir by the eryx-build C lane (manifest `vendor_patch`). Idempotent.
#
# wasi-libc's getsockname() aborts() on an unmappable host error instead of
# returning -1, so curl's set_local_ip() (called right after connect) crashes
# the whole program. The local-IP info it gathers is non-essential, so we
# compile that one call out. Everything else curl needs is handled by cflags
# in clis/curl.toml.
set -euo pipefail

f=lib/cf-socket.c
marker='HAVE_GETSOCKNAME: wasi-libc'

if grep -q "$marker" "$f"; then
    echo "patch-curl: already patched"
    exit 0
fi

sed -i \
    "s|^#ifdef HAVE_GETSOCKNAME\$|#if 0 /* $marker getsockname aborts on wasip2 */|" \
    "$f"

grep -q "$marker" "$f" || {
    echo "patch-curl: ERROR — failed to patch $f (upstream changed?)" >&2
    exit 1
}
echo "patch-curl: disabled getsockname in set_local_ip ($f)"
