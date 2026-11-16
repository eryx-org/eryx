#!/usr/bin/env bash
# Regenerate the vendored Node built-in polyfills (buffer.js, events.js,
# path.js, string_decoder.js): install the pinned npm packages, bundle each
# into one minified ES module with esbuild, and copy each package's licence.
# Each module starts with the full licences of the packages in it, so the
# notices ship inside the runtime wasm.
#
# Usage: crates/eryx-guest-js/src/node/regen.sh
# Needs network (the npm registry) and node; npx fetches the pinned esbuild.
# Run from anywhere.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# Every package that ends up in a module, pinned (dependencies too, so npm
# can't pick other versions).
packages=(
    buffer@6.0.3 base64-js@1.5.1 ieee754@1.2.1
    events@3.3.0
    path-browserify@1.0.1
    string_decoder@1.3.0 safe-buffer@5.2.1
    core-js-pure@3.51.0
)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
npm install --silent --no-save --ignore-scripts --no-audit --no-fund \
    --prefix "$work" "${packages[@]}"

rm -rf "$here/licenses"
mkdir -p "$here/licenses"
node "$here/regen.mjs" "$work/node_modules" "$here"
ls -l "$here"/*.js
