#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# WASI SDK location - check env var or local install
WASI_SDK_PATH="${WASI_SDK_PATH:-$PROJECT_ROOT/.wasi-sdk/wasi-sdk-27.0-x86_64-linux}"

if [[ ! -x "$WASI_SDK_PATH/bin/clang" ]]; then
    echo "Error: WASI SDK not found at $WASI_SDK_PATH"
    echo "Please install WASI SDK 27 or set WASI_SDK_PATH"
    exit 1
fi

CLANG="$WASI_SDK_PATH/bin/clang"
SYSROOT="$WASI_SDK_PATH/share/wasi-sysroot"

# Output directory
OUT_DIR="$SCRIPT_DIR/target"
mkdir -p "$OUT_DIR"

echo "Building eryx-wasm-runtime staticlib..."

# Build Rust staticlib with PIC (position-independent code)
# We need -Z build-std to rebuild std with PIC as well
cd "$SCRIPT_DIR"
RUSTFLAGS="-C relocation-model=pic" rustup run nightly cargo build \
    -Z build-std=panic_abort,std \
    --target wasm32-wasip1 \
    --release

STATICLIB="$PROJECT_ROOT/target/wasm32-wasip1/release/liberyx_wasm_runtime.a"

if [[ ! -f "$STATICLIB" ]]; then
    echo "Error: staticlib not found at $STATICLIB"
    exit 1
fi

echo "Compiling clock stubs..."

# Compile clock stubs (provides _CLOCK_*_CPUTIME_ID symbols that Rust libc needs)
"$CLANG" \
    --target=wasm32-wasip1 \
    --sysroot="$SYSROOT" \
    -fPIC \
    -c "$SCRIPT_DIR/clock_stubs.c" \
    -o "$OUT_DIR/clock_stubs.o"

echo "Linking with WASI SDK Clang to create .so..."

# Create shared library using Clang -shared
# This matches how componentize-py builds their runtime
# --allow-undefined lets libc symbols be resolved at final link time
"$CLANG" \
    --target=wasm32-wasip1 \
    --sysroot="$SYSROOT" \
    -shared \
    -Wl,--allow-undefined \
    -o "$OUT_DIR/liberyx_runtime.so" \
    -Wl,--whole-archive "$STATICLIB" -Wl,--no-whole-archive \
    "$OUT_DIR/clock_stubs.o"

echo "Checking output..."
wasm-tools print "$OUT_DIR/liberyx_runtime.so" 2>/dev/null | head -20 || file "$OUT_DIR/liberyx_runtime.so"

echo ""
echo "Built: $OUT_DIR/liberyx_runtime.so"
ls -la "$OUT_DIR/liberyx_runtime.so"
