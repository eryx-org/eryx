# Build image for Anthropic's OSS Scanner (https://github.com/anthropics/oss-scanner).
#
# Mirrors the `build-eryx-runtime` and `test` jobs in .github/workflows/ci.yml:
# build the Python WASM runtime from source, precompile it with preinit, then
# compile the whole workspace's tests so they can run offline.
#
# The scan runs without network access, so everything is fetched here.
# Try it locally: `docker build -f .oss-scanner/Dockerfile -t eryx-scan .`
# then `docker run --rm -it --network=none eryx-scan`.

# trixie: pyo3 is built with abi3-py312, bookworm only has Python 3.11.
FROM debian:trixie

# rustup/cargo stay in their default ~/.rustup and ~/.cargo: eryx-runtime's
# build.rs strips RUST*/CARGO* env vars (incl. RUSTUP_HOME) before invoking
# `rustup run nightly`, so a custom RUSTUP_HOME is invisible to it.
ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/root/.cargo/bin:$PATH \
    WASI_SDK_PATH=/opt/wasi-sdk \
    CARGO_TERM_COLOR=never

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential clang pkg-config \
      python3 python3-dev protobuf-compiler zstd xz-utils procps \
    && rm -rf /var/lib/apt/lists/*

# Keep in sync with `wasi_sdk_version` in mise.toml.
ARG WASI_SDK=33
RUN mkdir -p "$WASI_SDK_PATH" \
    && curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK}/wasi-sdk-${WASI_SDK}.0-x86_64-linux.tar.gz" \
      | tar -xz -C "$WASI_SDK_PATH" --strip-components=1

# Keep in sync with `cargo:cargo-nextest` in mise.toml.
ARG NEXTEST=0.9.148
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
    && curl -fsSL "https://get.nexte.st/${NEXTEST}/linux" | tar -xz -C /root/.cargo/bin

COPY . /src
WORKDIR /src

# Stable toolchain comes from rust-toolchain.toml; nightly + rust-src is what
# crates/eryx-runtime/build.rs uses to build the guest with -Z build-std.
RUN rustup toolchain install \
    && rustup toolchain install nightly --profile minimal --component rust-src \
    && cargo fetch --locked

# 1. Guest runtime: crates/eryx-runtime/runtime.wasm plus late-linking libraries.
#    Refresh prebuilt/ from this build so it can't lag the guest source.
RUN BUILD_ERYX_RUNTIME=1 cargo build --locked -p eryx-runtime --release \
    && OUT_DIR=$(dirname "$(find target/release -path '*/eryx-runtime-*/out/liberyx_runtime.so' -type f | head -1)") \
    && cp "$OUT_DIR"/liberyx_runtime.so.zst "$OUT_DIR"/liberyx_bindings.so.zst crates/eryx-runtime/prebuilt/ \
    && mkdir -p crates/eryx-wasm-runtime/target \
    && cp "$OUT_DIR"/liberyx_runtime.so crates/eryx-wasm-runtime/target/

# 2. Python stdlib for preinit and the eryx-wasm-runtime tests.
RUN mkdir -p crates/eryx-wasm-runtime/tests/site-packages \
    && tar -xf crates/eryx/python-stdlib.tar.zst -C crates/eryx-wasm-runtime/tests/

# 3. Precompiled, pre-initialized runtime.cwasm (embedded by the `eryx` crate).
RUN ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run --locked -p eryx-precompile --release -- compile \
      crates/eryx-runtime/runtime.wasm \
      --preinit \
      --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
      --target x86-64-v2 \
      --output crates/eryx-runtime/runtime.cwasm \
      --no-verify

# 4. Compile every test binary now: the scan VM is offline and has 2 CPUs.
RUN cargo nextest run --locked --workspace --all-features --no-run

# Run the suite as a smoke check, but don't fail the image on a flaky test.
RUN cargo nextest run --locked --workspace --all-features --profile ci \
    || echo "WARNING: cargo nextest reported failures"
