#!/usr/bin/env bash
# Add precompiled bytecode (__pycache__/*.pyc) to the bundled Python stdlib.
#
# The stdlib is mounted read-only in the sandbox, so Python can never write its
# own .pyc files; without these, every import outside the pre-init snapshot
# recompiles from source and burns fuel (#727).
#
# Run this after changing the stdlib contents, with a python3.14 on PATH (the
# .pyc magic number must match the guest's CPython, or imports silently fall
# back to compiling). Rewrites both archives in crates/eryx/.
#
# Needs GNU tar and zstd.
#
# Uses unchecked-hash .pyc files: they are never revalidated against the
# source, so the archives must be regenerated together with any .py change.
#
# `--check` regenerates into a temp dir and fails if either archive's contents
# differ (CI uses this). It compares extracted files, not archive bytes, so
# zstd/gzip versions don't matter; pin the CPython patch version in CI, since
# a different one can marshal different bytecode.

set -euo pipefail

cd "$(dirname "$0")/../crates/eryx"

check=0
[[ "${1:-}" == "--check" ]] && check=1

PYTHON="${PYTHON:-python3.14}"
"$PYTHON" -c 'import sys; assert sys.version_info[:2] == (3, 14), sys.version' \
    || { echo "need CPython 3.14 (set PYTHON=...)"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

tar -I zstd -xf python-stdlib.tar.zst -C "$work"
find "$work" -name __pycache__ -prune -exec rm -rf {} +
# -j is pinned: marshal output differs between serial (-j 1) and parallel
# compilation, so a machine-dependent worker count would break --check.
# -s/-p record the in-sandbox path (/python-stdlib/...) as each file's name,
# so the bytecode doesn't embed this temp dir and tracebacks match the guest.
"$PYTHON" -m compileall -q -j 4 --invalidation-mode unchecked-hash \
    -s "$work" -p / "$work/python-stdlib"

if [[ $check == 1 ]]; then
    rc=0
    for archive in zst gz; do
        old="$(mktemp -d -p "$work")"
        tar -xf "python-stdlib.tar.$archive" -C "$old"
        diff -r "$old/python-stdlib" "$work/python-stdlib" > /dev/null \
            || { echo "python-stdlib.tar.$archive is out of date; run scripts/compile-stdlib.sh"; rc=1; }
    done
    exit $rc
fi

# Sorted names + fixed owner/mtime keep the archives reproducible.
pack() {
    tar -C "$work" --sort=name --owner=0 --group=0 --numeric-owner \
        --mtime='2020-01-01 00:00:00' -cf - python-stdlib
}
pack | zstd -q -19 -f -o python-stdlib.tar.zst
pack | gzip -9n > python-stdlib.tar.gz
