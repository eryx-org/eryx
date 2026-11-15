#!/usr/bin/env bash
# Smoke-test a built eryx-bundler.wasm: bundle the fixture packages under
# wasmtime, mounting only the fixture (read-only) and an output dir, and check
# conditions, externals, polyfills, code splitting and failure exits.
# Usage: crates/eryx-bundler/smoke.sh [path/to/eryx-bundler.wasm]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
wasm="${1:-$here/target/wasm32-wasip2/release/eryx-bundler.wasm}"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

run() {
    wasmtime run --dir "$here/tests/fixture::/src" --dir "$out::/out" "$wasm" "$@"
}
fail() { echo "smoke: $*" >&2; exit 1; }

run --cwd /src --out /out \
    --entry esm-pkg --entry esm-pkg/extra --entry uses-builtins --entry @scope/sub

report="$out/bundle.json"
for entry in '"esm-pkg": "esm-pkg.js"' '"esm-pkg/extra": "esm-pkg__extra.js"' \
    '"uses-builtins": "uses-builtins.js"' '"@scope/sub": "@scope__sub.js"' '"bundler": "rolldown 1.3.2"'; do
    grep -qF "$entry" "$report" || fail "bundle.json lacks $entry: $(cat "$report")"
done
grep -q 'target = "browser"' "$out/esm-pkg.js" || fail "the browser condition wasn't used"
! grep -q '"node"' "$out/esm-pkg.js" || fail "the node build was bundled"
grep -q 'from "node:fs"' "$out/uses-builtins.js" || fail "node:fs wasn't left external"
grep -q 'from "path"' "$out/uses-builtins.js" || fail "path wasn't left external"
grep -q 'node_modules/events/events.js' "$out/uses-builtins.js" || fail "the installed events polyfill wasn't bundled"
# cjs-pkg is shared by esm-pkg and @scope/sub, so it's split into its own chunk.
[ "$(grep -l 'n \* 2' "$out"/*.js | wc -l)" -eq 1 ] || fail "shared code wasn't split out once"

# `browser` field mappings and nested polyfills beat Node built-ins.
run --cwd /src --out /out --entry browser-false --entry browser-shim \
    --entry needs-buffer --entry env-pkg --entry needs-missing
! grep -q '"fs"' "$out/browser-false.js" || fail "browser: {fs: false} wasn't applied: $(cat "$out/browser-false.js")"
grep -q '(shim)' "$out/browser-shim.js" || fail "the browser shim for path wasn't bundled"
! grep -q 'from "path"' "$out/browser-shim.js" || fail "path stayed external despite the browser shim"
grep -q 'nested buffer polyfill' "$out/needs-buffer.js" || fail "the nested buffer polyfill wasn't bundled"
! grep -q 'from "buffer"' "$out/needs-buffer.js" || fail "buffer stayed external despite the nested polyfill"
# process.env.NODE_ENV is "production".
! grep -q 'process.env' "$out/env-pkg.js" || fail "process.env.NODE_ENV wasn't defined"
grep -q 'prod build' "$out/env-pkg.js" || fail "the production branch wasn't kept"
# An unresolvable bare import stays external, with a warning (exit 0).
grep -q 'from "not-installed"' "$out/needs-missing.js" || fail "the unresolved import isn't external"
grep -qi 'not-installed' "$out/bundle.json" || fail "no warning names the unresolved import: $(cat "$out/bundle.json")"

# A package that isn't installed fails with status 1 and a diagnostic.
set +e
msg="$(run --cwd /src --out /out --entry missing-pkg 2>&1)"
status=$?
set -e
[ "$status" -eq 1 ] || fail "a missing package exited $status: $msg"
echo "$msg" | grep -qi 'missing-pkg' || fail "no diagnostic names the missing package: $msg"

# Bad arguments fail with a usage message.
set +e
msg="$(run --out /out 2>&1)"
status=$?
set -e
[ "$status" -ne 0 ] && echo "$msg" | grep -q 'usage:' || fail "bad arguments exited $status: $msg"

echo "smoke: ok ($(ls "$out" | wc -l) files)"
