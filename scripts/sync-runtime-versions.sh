#!/usr/bin/env bash
# Lock the pyeryx companion packages to the workspace version.
#
# A precompiled runtime only loads in the eryx version that built it, so
# pyeryx-runtime-javascript and pyeryx-runtime-shell are released at exactly
# pyeryx's version (the workspace version in Cargo.toml), and pyeryx's
# `javascript`/`shell`/`all` extras pin them with `==`. This rewrites the
# companions' versions, the pins and the uv.lock entries to match.
# tests/test_languages.py fails if they drift.
#
# Usage: scripts/sync-runtime-versions.sh

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON_DIR="$ROOT_DIR/crates/eryx-python"

# The `version = "..."` line under [workspace.package].
VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$ROOT_DIR/Cargo.toml")
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-].+)?$ ]]; then
  echo "error: no workspace version in Cargo.toml (got '$VERSION')" >&2
  exit 1
fi

for lang in javascript shell; do
  pkg="pyeryx-runtime-$lang"
  sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$PYTHON_DIR/runtimes/$lang/pyproject.toml"
  sed -i.bak -E "s/\"$pkg==[^\"]*\"/\"$pkg==$VERSION\"/g" "$PYTHON_DIR/pyproject.toml"
  # The [[package]] entry: its version is the line after the name.
  sed -i.bak -E "/^name = \"$pkg\"$/{n;s/^version = \"[^\"]*\"/version = \"$VERSION\"/;}" "$PYTHON_DIR/uv.lock"
done
rm -f "$PYTHON_DIR"/runtimes/*/pyproject.toml.bak "$PYTHON_DIR/pyproject.toml.bak" "$PYTHON_DIR/uv.lock.bak"

echo "pyeryx runtime companions locked to $VERSION"
