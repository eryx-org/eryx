#!/usr/bin/env bash
# Regenerate the bundled npm packages tests/js_environment.rs imports: install
# the pinned versions with `npm install --ignore-scripts`, bundle them
# minified with the eryx-bundler wasm (mise run build-bundler), and copy each
# package's licence.
#
# Usage: crates/eryx/tests/fixtures/js-environment/regen.sh
# Needs network (the npm registry), npm and wasmtime. Run from anywhere.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../../.." && pwd)"
packages=(zod@4.6.7 date-fns@4.4.0 lodash@4.18.1 uuid@14.0.3 nanoid@6.0.2)
entries=(zod date-fns lodash uuid nanoid)

tree="$(mktemp -d)"
trap 'rm -rf "$tree"' EXIT
cd "$root"
mise run build-bundler >/dev/null
echo '{"private": true}' > "$tree/package.json"
npm install --prefix "$tree" --ignore-scripts --omit=dev --no-audit --no-fund --save-exact \
    "${packages[@]}" >/dev/null
[ -d "$tree/node_modules" ] || { echo "install failed" >&2; exit 1; }

rm -rf "$here/modules" "$here/licenses"
mkdir -p "$here/modules" "$here/licenses"
args=(--cwd /src --out /out --minify)
for entry in "${entries[@]}"; do args+=(--entry "$entry"); done
wasmtime run --dir "$tree::/src" --dir "$here/modules::/out" \
    crates/eryx-runtime/eryx-bundler.wasm "${args[@]}"
mv "$here/modules/bundle.json" "$here/bundle.json"

for entry in "${entries[@]}"; do
    license="$(find "$tree/node_modules/$entry" -maxdepth 1 -iname 'licen[cs]e*' | head -1)"
    [ -n "$license" ] || { echo "no licence file for $entry" >&2; exit 1; }
    cp "$license" "$here/licenses/$entry-$(basename "$license")"
done
du -sh "$here/modules" "$here/licenses"
