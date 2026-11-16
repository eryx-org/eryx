#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Patch the git source tree for the wasip2 C lane: apply git-wasi.patch (the
# run-command.c backend over eryx:process/run, and two small WASI fixes).
# Run from the git source dir by the eryx-build C lane (manifest
# `vendor_patch`). Idempotent. GPL-2.0-only, like git; see ../NOTICE.md.
set -euo pipefail

patch="$(dirname "${BASH_SOURCE[0]}")/git-wasi.patch"
if git apply --reverse --check "$patch" 2>/dev/null; then
    echo "patch-git: already patched"
    exit 0
fi
git apply "$patch"
