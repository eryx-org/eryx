#!/bin/sh
# Downloads the pinned unkernel kernel and Alpine image into ./vendor, and
# checks them. Neither is committed: the kernel is GPL-2.0-only (see README).
set -eu
TAG=v0.2.0-261124-c08e977
BASE=https://github.com/pithings/unkernel/releases/download/$TAG
cd "$(dirname "$0")"
mkdir -p vendor
fetch() {
    [ -f "vendor/$1" ] || curl -fsSL -o "vendor/$1" "$BASE/$1"
    echo "$2  vendor/$1" | sha256sum -c -
}
fetch unkernel.wasm b11c542fbf1f84eafa50c0500b075a3a1978bf688d694c652e8e6822d8bfe43c
fetch alpine.erofs bb9bed60c5d5418dc35b23ed69112882df7a7c5ae878490fc45a82fc5c28ffab
