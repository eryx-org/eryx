#!/usr/bin/env bash
# Bump version for eryx release using release-plz
# Usage: ./scripts/bump-version.sh 0.3.0

set -euo pipefail

NEW_VERSION="${1:-}"

if [[ -z "$NEW_VERSION" ]]; then
    echo "Usage: $0 <new-version>"
    echo "Example: $0 0.3.0"
    exit 1
fi

# Validate semver format
if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: Version must be in semver format (e.g., 0.3.0)"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

echo "Bumping all packages to $NEW_VERSION using release-plz..."

cd "$ROOT_DIR"
release-plz set-version "$NEW_VERSION"

# Sync JS package version
JS_PKG="$ROOT_DIR/js/eryx/package.json"
if [[ -f "$JS_PKG" ]]; then
  echo "Syncing JS package version to $NEW_VERSION..."
  jq --arg v "$NEW_VERSION" '.version = $v' "$JS_PKG" > "$JS_PKG.tmp"
  mv "$JS_PKG.tmp" "$JS_PKG"
fi

echo ""
echo "Version bumped to $NEW_VERSION"
echo ""
echo "Next steps:"
echo "  1. Review changes: git diff"
echo "  2. Run formatter: mise run fmt"
echo "  3. Commit: git commit -am 'chore: release v$NEW_VERSION'"
echo "  4. Push to the release-plz PR branch"
