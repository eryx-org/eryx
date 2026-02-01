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

echo ""
echo "Version bumped to $NEW_VERSION"
echo ""
echo "Next steps:"
echo "  1. Review changes: git diff"
echo "  2. Run formatter: mise run fmt"
echo "  3. Commit: git commit -am 'chore: release v$NEW_VERSION'"
echo "  4. Push to the release-plz PR branch"
