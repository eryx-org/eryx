#!/usr/bin/env bash
# Smoke-test a built CLI component under the wasmtime CLI, from the repo root:
#   clis/smoke.sh <name>      (after `mise run build-cli -- <name>`)
#
# This only proves the component runs. Running commands inside eryx is #809.
set -euo pipefail

NAME="${1:?Usage: clis/smoke.sh <name>}"
OUT="scratch/$NAME-component"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/data"
printf 'banana\napple\ncherry\n' > "$WORK/data/fruit.txt"

# Fail unless $1 (the output) contains the fixed string $2.
expect() {
    printf '%s\n' "$1"
    grep -qF -- "$2" <<< "$1" || { echo "::error::$NAME: expected '$2' in the output above"; exit 1; }
}

case "$NAME" in
coreutils)
    # The precompiled cwasm needs the engine flags it was compiled with.
    cu() {
        wasmtime run --allow-precompiled -W component-model-async \
            --dir "$WORK/data::/data" --argv0 "$1" "$OUT/coreutils.cwasm" "${@:2}"
    }
    expect "$(cu sort /data/fruit.txt | head -1)" apple
    expect "$(cu wc -l /data/fruit.txt)" 3
    expect "$(cu ls /data)" fruit.txt
    ;;
rg)
    expect "$(wasmtime run --dir "$WORK/data::/data" "$OUT/component.wasm" -n an /data/fruit.txt)" "1:banana"
    ;;
sd)
    expect "$(printf 'hello world\n' | wasmtime run "$OUT/component.wasm" world eryx)" "hello eryx"
    ;;
jaq)
    expect "$(printf '{"a":1}' | wasmtime run "$OUT/component.wasm" .a)" 1
    expect "$(printf '[1,2,3]' | wasmtime run "$OUT/component.wasm" -c 'map(. * 2)')" "[2,4,6]"
    # In-place edit: the file changes and jaq exits 0.
    printf '{"a":1}' > "$WORK/data/edit.json"
    wasmtime run --dir "$WORK/data::/data" "$OUT/component.wasm" -c -i '.a = 2' /data/edit.json
    expect "$(cat "$WORK/data/edit.json")" '{"a":2}'
    ;;
sqlite3)
    expect "$(wasmtime run "$OUT/component.wasm" :memory: 'select 6*7;')" 42
    ;;
curl)
    net=(-S inherit-network -S allow-ip-name-lookup)
    expect "$(wasmtime run "$OUT/component.wasm" --version)" "curl 8.20.0"
    expect "$(wasmtime run "${net[@]}" "$OUT/component.wasm" -sSI http://example.com)" "HTTP/1.1 200"
    # WASI doesn't follow symlinks, and the system bundle usually is one, so
    # copy the real file in.
    mkdir -p "$WORK/certs"
    cp "$(readlink -f /etc/ssl/certs/ca-certificates.crt)" "$WORK/certs/ca.crt"
    expect "$(wasmtime run "${net[@]}" --dir "$WORK/certs::/certs" "$OUT/component.wasm" \
        -sSI --cacert /certs/ca.crt https://example.com)" " 200"
    ;;
gh)
    # gh resolves names itself, so it needs the real resolv.conf (WASI won't
    # follow the usual symlink) and a CA bundle under /etc.
    mkdir -p "$WORK/etc" "$WORK/home"
    cp "$(readlink -f /etc/resolv.conf)" "$WORK/etc/resolv.conf"
    cp "$(readlink -f /etc/ssl/certs/ca-certificates.crt)" "$WORK/etc/ca-certificates.crt"
    ghw() {
        wasmtime run -S p3 -W component-model-async -W component-model-more-async-builtins \
            -W max-wasm-stack=8388608 -S tcp -S udp -S inherit-network -S allow-ip-name-lookup \
            --dir "$WORK/etc::/etc" --dir "$WORK/home::/home/gh" \
            --env HOME=/home/gh --env SSL_CERT_FILE=/etc/ca-certificates.crt \
            --env GH_NO_UPDATE_NOTIFIER=1 --env TERM=dumb "$@" 2>&1
    }
    expect "$(ghw "$OUT/component.wasm" --version)" "gh version 2.87.3"
    # A placeholder token gets a real 401 from api.github.com: DNS, TCP and TLS
    # all work, and no secret is needed.
    expect "$(ghw --env GH_TOKEN=placeholder "$OUT/component.wasm" api /zen)" "HTTP 401"
    ;;
*)
    echo "no smoke test for '$NAME'" >&2
    exit 1
    ;;
esac
echo "$NAME: smoke test passed"
