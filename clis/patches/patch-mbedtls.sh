#!/usr/bin/env bash
# Patch the mbedTLS source for the wasip2 C lane (curl TLS). Run from the
# mbedTLS source dir by the eryx-build C lane (dep `config_patch`). Idempotent.
#
# Disable two default-on modules that only build on Unix/Windows and that curl
# does not need:
#   - MBEDTLS_TIMING_C — alarm/signal-based timing (tests/benchmarks).
#   - MBEDTLS_NET_C    — BSD-socket BIO; curl provides its own socket transport.
# (Entropy + ms-time are handled via alt-hooks in the WASI shim, enabled by
# cflags in clis/curl.toml.)
set -euo pipefail

cfg=include/mbedtls/mbedtls_config.h
[ -f "$cfg" ] || { echo "patch-mbedtls: $cfg not found (wrong dir?)" >&2; exit 1; }

sed -i 's|^#define MBEDTLS_TIMING_C|//#define MBEDTLS_TIMING_C  /* wasi: no Unix timing */|' "$cfg"
sed -i 's|^#define MBEDTLS_NET_C|//#define MBEDTLS_NET_C  /* wasi: curl provides its own socket BIO */|' "$cfg"

if grep -qE '^#define MBEDTLS_(TIMING|NET)_C' "$cfg"; then
    echo "patch-mbedtls: ERROR — failed to disable TIMING_C/NET_C (upstream changed?)" >&2
    exit 1
fi
echo "patch-mbedtls: disabled MBEDTLS_TIMING_C + MBEDTLS_NET_C"
