# syntax=docker/dockerfile:1
#
# Build arg: "default" precompiles from source, "custom" uses a pre-built .cwasm.
# In CI, use RUNTIME_SOURCE=custom with the artifact from build-eryx-runtime.
ARG RUNTIME_SOURCE=default

# ---------- Stage 1: Precompile WASM runtime (only when RUNTIME_SOURCE=default) ----------
FROM rust:1.91-bookworm AS precompile

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests.
COPY Cargo.toml Cargo.lock ./

# Copy all workspace crates that affect precompilation.
COPY crates/eryx/ crates/eryx/
COPY crates/eryx-precompile/ crates/eryx-precompile/
COPY crates/eryx-runtime/ crates/eryx-runtime/
COPY crates/eryx-wasm-runtime/ crates/eryx-wasm-runtime/
COPY crates/eryx-linker-wasm/ crates/eryx-linker-wasm/
COPY crates/eryx-macros/ crates/eryx-macros/
COPY crates/eryx-python/ crates/eryx-python/
COPY crates/eryx-vfs/ crates/eryx-vfs/
COPY crates/wit-dylib-ffi/ crates/wit-dylib-ffi/

# Stub for eryx-server (not compiled here, but needed for workspace resolution).
COPY crates/eryx-server/Cargo.toml crates/eryx-server/Cargo.toml
COPY crates/eryx-server/proto/ crates/eryx-server/proto/
RUN mkdir -p crates/eryx-server/src && echo "fn main() {}" > crates/eryx-server/src/main.rs

# Extract Python stdlib for pre-initialization.
RUN mkdir -p crates/eryx-wasm-runtime/tests/python-stdlib && \
    tar -xf crates/eryx/python-stdlib.tar.zst \
      -C crates/eryx-wasm-runtime/tests/ \
      --strip-components=0

# Build eryx-precompile and produce runtime.cwasm with preinit.
ARG ERYX_CPU_FEATURES=x86-64-v3
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    ERYX_PRECOMPILE_BOOTSTRAP=1 \
    cargo run --release -p eryx-precompile -- compile \
      crates/eryx-runtime/runtime.wasm \
      --preinit \
      --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
      --target "${ERYX_CPU_FEATURES}" \
      --output crates/eryx-runtime/runtime.cwasm \
      --no-verify

# Runtime provider: default (from precompile stage output)
FROM precompile AS runtime-default

# Runtime provider: custom (from build context file)
FROM scratch AS runtime-custom
ARG RUNTIME_CWASM=runtime.cwasm
COPY ${RUNTIME_CWASM} /build/crates/eryx-runtime/runtime.cwasm

# Select runtime based on RUNTIME_SOURCE arg
FROM runtime-${RUNTIME_SOURCE} AS runtime

# ---------- Stage 2: Build the server binary ----------
FROM rust:1.91-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy full workspace source.
COPY . .

# Copy runtime.cwasm from selected runtime stage AFTER source copy so it
# isn't overwritten by any runtime.cwasm that exists in the source tree.
COPY --from=runtime /build/crates/eryx-runtime/runtime.cwasm crates/eryx-runtime/runtime.cwasm

# Build the server with preinit support using BuildKit cache mounts.
# - cargo registry cache: avoids re-downloading crates
# - target dir cache: enables incremental compilation across builds
# We copy the final binary out of the cache mount since it disappears after RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p eryx-server --features eryx/embedded,eryx/preinit \
    && cp target/release/eryx-server /usr/local/bin/eryx-server

# ---------- Stage 3: Runtime image ----------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051

ENTRYPOINT ["eryx-server"]
