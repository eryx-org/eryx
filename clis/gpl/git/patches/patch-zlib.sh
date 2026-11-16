#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Name zlib's static library libz.a, as git links it (`-lz`): zlib's CMake
# only does that `if(UNIX)`, which the wasi-sdk toolchain isn't. Run from
# the zlib source dir by the eryx-build C lane (dep `config_patch`).
# Idempotent.
set -euo pipefail

marker='# eryx-build: libz.a on WASI'
if grep -qF "$marker" CMakeLists.txt; then
    echo "patch-zlib: already patched"
    exit 0
fi
printf '\n%s\nset_target_properties(zlibstatic PROPERTIES OUTPUT_NAME z)\n' "$marker" >> CMakeLists.txt
