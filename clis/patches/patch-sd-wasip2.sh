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
set -euo pipefail

[ -f Cargo.lock ] || { echo "patch-sd-wasip2: Cargo.lock not found (wrong dir?)" >&2; exit 1; }

# rustix: bump the 0.38.x line (terminal_size's dep) to a wasip2-clean release.
cargo update -p rustix@0.38 --precise 0.38.44
# tempfile: 3.27.0 builds on wasip2 (and pulls rustix 1.x for its own use).
cargo update -p tempfile --precise 3.27.0

echo "patch-sd-wasip2: lockfile advanced (rustix 0.38.44, tempfile 3.27.0)"
