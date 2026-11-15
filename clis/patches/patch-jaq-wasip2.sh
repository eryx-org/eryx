#!/usr/bin/env bash
# Patch jaq for the wasm32-wasip2 Rust lane. Run from the jaq source dir by the
# eryx-build Rust lane (manifest `vendor_patch`). Idempotent.
#
# - jaq's `repl` filter uses rustyline, which has no WASI backend (it needs a
#   terminal and fd-lock). Leave rustyline out on WASI and drop that one
#   filter; everything else is plain jq.
# - tempfile is too old for wasip2 (below).
set -euo pipefail

toml=jaq/Cargo.toml
main=jaq/src/main.rs
[ -f "$toml" ] && [ -f "$main" ] || { echo "patch-jaq-wasip2: jaq sources not found (wrong dir?)" >&2; exit 1; }

# tempfile before 3.27 uses std::os::wasi directly, which is unstable on
# wasip2; advance the lockfile to a wasip2-clean release (as for sd).
cargo update -p tempfile --precise 3.27.0

if grep -q 'eryx: no repl on WASI' "$main"; then
    echo "patch-jaq-wasip2: already patched"
    exit 0
fi

# rustyline only off WASI.
line=$(grep -E '^rustyline = ' "$toml")
grep -vE '^rustyline = ' "$toml" > "$toml.tmp" && mv "$toml.tmp" "$toml"
printf '\n[target.'"'"'cfg(not(target_os = "wasi"))'"'"'.dependencies]\n%s\n' "$line" >> "$toml"

# On WASI, `funs` is jaq's standard functions without `repl`.
perl -0pi -e 's/^mod funs;$/#[cfg(not(target_os = "wasi"))]\nmod funs;\n#[cfg(target_os = "wasi")] \/\/ eryx: no repl on WASI\nmod funs {\n    pub fn funs() -> impl Iterator<Item = jaq_all::jaq_core::native::Fun<jaq_all::data::DataKind>> {\n        jaq_all::data::funs()\n    }\n}/m' "$main"

grep -q 'eryx: no repl on WASI' "$main" || { echo "patch-jaq-wasip2: ERROR — failed to patch $main (upstream changed?)" >&2; exit 1; }
echo "patch-jaq-wasip2: dropped rustyline and the repl filter on WASI"
