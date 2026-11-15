#!/usr/bin/env bash
# Patch jaq for the wasm32-wasip2 Rust lane. Run from the jaq source dir by the
# eryx-build Rust lane (manifest `vendor_patch`). Idempotent.
#
# - jaq's `repl` filter uses rustyline, which has no WASI backend (it needs a
#   terminal and fd-lock). Leave rustyline out on WASI and drop that one
#   filter; everything else is plain jq.
# - tempfile is too old for wasip2, and `jaq -i` fails setting permissions
#   (below).
set -euo pipefail

toml=jaq/Cargo.toml
main=jaq/src/main.rs
[ -f "$toml" ] && [ -f "$main" ] || { echo "patch-jaq-wasip2: jaq sources not found (wrong dir?)" >&2; exit 1; }

# tempfile before 3.27 uses std::os::wasi directly, which is unstable on
# wasip2; advance the lockfile to a wasip2-clean release (as for sd).
cargo update -p tempfile --precise 3.27.0

# `jaq -i` copies the file's permissions onto the rewritten file, but WASI has
# no permissions to set (ENOSYS), so the edit lands and jaq still exits 1.
# Skip that on WASI.
if ! grep -q 'eryx: no permissions on WASI' "$main"; then
    perl -0pi -e 's/^(\s*)(let perms = std::fs::metadata\(path\)\?\.permissions\(\);)$/$1#[cfg(not(target_os = "wasi"))] \/\/ eryx: no permissions on WASI\n$1$2/m; s/^(\s*)(std::fs::set_permissions\(path, perms\)\?;)$/$1#[cfg(not(target_os = "wasi"))]\n$1$2/m' "$main"
    if ! grep -q 'eryx: no permissions on WASI' "$main" ||
        ! grep -B1 'std::fs::set_permissions(path, perms)' "$main" | grep -q 'cfg(not(target_os = "wasi"))'; then
        echo "patch-jaq-wasip2: ERROR — failed to gate set_permissions in $main (upstream changed?)" >&2
        exit 1
    fi
fi

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
