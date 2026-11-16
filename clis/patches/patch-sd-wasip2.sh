#!/usr/bin/env bash
# Patch sd for the wasm32-wasip2 Rust lane. Run from the sd source dir by
# the eryx-build Rust lane (manifest `vendor_patch`). Idempotent.
#
# sd's pinned Cargo.lock predates wasip2 support in two transitive deps:
#   - tempfile 3.8.0  uses std::os::wasi directly (gated behind the unstable
#     `wasip2` library feature) — 3.27.0 builds clean on wasip2.
#   - rustix 0.38.20  (via terminal_size → clap) likewise — 0.38.44 is fine.
# RUSTC_BOOTSTRAP=1 alone can't help: it enables nightly features but the crate
# source must still opt in with `#![feature(wasip2)]`, which we can't add to a
# registry crate. The fix is to advance the lockfile to versions that compile on
# wasip2 with the stable toolchain. Pin precise versions for reproducibility.
#
# It also gives sd-cli the licence field it's missing (below).
set -euo pipefail

[ -f Cargo.lock ] || { echo "patch-sd-wasip2: Cargo.lock not found (wrong dir?)" >&2; exit 1; }

# rustix: bump the 0.38.x line (terminal_size's dep) to a wasip2-clean release.
cargo update -p rustix@0.38 --precise 0.38.44
# tempfile: 3.27.0 builds on wasip2 (and pulls rustix 1.x for its own use).
cargo update -p tempfile --precise 3.27.0

echo "patch-sd-wasip2: lockfile advanced (rustix 0.38.44, tempfile 3.27.0)"

# sd-cli, the package that builds the `sd` binary, doesn't declare the
# workspace's MIT licence, so the third-party notices can't tell what it is
# (#834). Declare it.
if ! grep -q '^license' sd-cli/Cargo.toml; then
    perl -0pi -e 's/^(edition\.workspace = true)$/$1\nlicense.workspace = true/m' sd-cli/Cargo.toml
    grep -q '^license.workspace = true' sd-cli/Cargo.toml || {
        echo "patch-sd-wasip2: ERROR — failed to add sd-cli's licence (upstream changed?)" >&2
        exit 1
    }
    echo "patch-sd-wasip2: sd-cli declares the workspace licence"
fi
