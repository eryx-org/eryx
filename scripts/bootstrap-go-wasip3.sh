#!/usr/bin/env bash
# Bootstrap the pinned wasip3 Go toolchain (our controlled fork of
# jellevandenhooff/go, ported to WASI 0.3.0 as implemented by the wasmtime
# version eryx uses). Clones + builds it from source, then prints the
# resulting GOROOT on the last line so callers can `export ERYX_GO_WASIP3_ROOT`.
#
# Building from source takes a few minutes; CI caches the output keyed on this
# script's hash, which covers GO_WASIP3_COMMIT.
#
# Usage:
#   GOROOT_BOOTSTRAP=$(go env GOROOT) scripts/bootstrap-go-wasip3.sh [dest-dir]
set -euo pipefail

# --- Pinned toolchain (bump these together when moving the fork) ---
GO_WASIP3_REPO="https://github.com/sd2k/go.git"
GO_WASIP3_REF="wasip3-0.3.0-go1.27.4"
GO_WASIP3_COMMIT="dfb22bf4bbf14406a17de2ec48cc9af3e45649a0"

DEST="${1:-scratch/go-wasip3}"

log() { echo "[bootstrap-go-wasip3] $*" >&2; }

if [ -x "$DEST/bin/go" ] && [ "$(git -C "$DEST" rev-parse HEAD 2>/dev/null)" = "$GO_WASIP3_COMMIT" ]; then
    log "toolchain already present at $DEST @ $GO_WASIP3_COMMIT"
    readlink -f "$DEST"
    exit 0
fi

if [ ! -d "$DEST/.git" ]; then
    log "cloning $GO_WASIP3_REPO ($GO_WASIP3_REF) into $DEST"
    git clone --branch "$GO_WASIP3_REF" "$GO_WASIP3_REPO" "$DEST"
fi
git -C "$DEST" fetch origin "$GO_WASIP3_REF"
git -C "$DEST" checkout --quiet "$GO_WASIP3_COMMIT"

log "building the toolchain (make.bash)…"
# make.bash logs to stdout; redirect to stderr so this script's stdout is
# only the final GOROOT path (callers capture it).
( cd "$DEST/src" && GOTOOLCHAIN=local bash make.bash ) >&2

if [ ! -x "$DEST/bin/go" ]; then
    log "ERROR: build did not produce $DEST/bin/go"
    exit 1
fi
log "done"
readlink -f "$DEST"
