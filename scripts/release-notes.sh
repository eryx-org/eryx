#!/usr/bin/env bash
# Print the GitHub release notes for an eryx release: eryx's section of the
# shared CHANGELOG.md, then the section of every other crate tagged at the
# same commit, under its own heading. release-plz can't do this itself: it only
# finds headings that start with a version, and ours start with the crate name.
# Usage: scripts/release-notes.sh <eryx-vX.Y.Z tag> [changelog]
set -euo pipefail
tag=$1 file=${2:-CHANGELOG.md}

# One crate's section, without its heading; fails if there is none.
section() {
  awk -v head="## \`$1\` - [$2]" '
    index($0, head) == 1 { found = 1; next }
    found && /^## / { exit }
    found { print }
    END { if (!found) exit 1 }
  ' "$file" | sed -e '/./,$!d'
}

section eryx "${tag#eryx-v}"
git tag --points-at "$tag^{commit}" | sort | while read -r t; do
  pkg=${t%-v*} ver=${t##*-v}
  [[ $pkg == eryx ]] && continue
  # Tags without a section (no changes, or not a crate) are skipped.
  notes=$(section "$pkg" "$ver") || continue
  printf "\n## \`%s\` %s\n\n%s\n" "$pkg" "$ver" "$notes"
done
