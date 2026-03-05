# Build arg: "default" precompiles from source, "custom" uses a pre-built .cwasm.
ARG RUNTIME_SOURCE=default

# Pre-built cargo-chef binary.
FROM lukemathwalker/cargo-chef:latest-rust-1.90-bookworm AS chef

# Stage 1: Generate cargo-chef recipe from workspace.
FROM chef AS planner

WORKDIR /build
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 2: Build dependencies (cached until Cargo.toml/Cargo.lock change).
FROM chef AS cook

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY --from=planner /build/recipe.json recipe.json

# Proto files are needed by eryx-server/build.rs (tonic_build).
COPY crates/eryx-server/proto/ crates/eryx-server/proto/

# Cook dependencies in bootstrap mode (no runtime.cwasm yet).
RUN ERYX_PRECOMPILE_BOOTSTRAP=1 \
    cargo chef cook --release --recipe-path recipe.json \
      -p eryx-server -p eryx-precompile \
      --features eryx/embedded,eryx/preinit

# Stage 3: Precompile WASM runtime (cached unless non-server crates change).
FROM rust:1.90-bookworm AS precompile

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Bring in pre-built dependencies from cook stage.
COPY --from=cook /build/target target

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
RUN ERYX_PRECOMPILE_BOOTSTRAP=1 \
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

# Stage 4: Build the server (only recompiles when server code changes).
FROM rust:1.90-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Bring in pre-built dependencies from cook stage.
COPY --from=cook /build/target target

# Copy runtime.cwasm from selected runtime stage so build.rs can embed it.
COPY --from=runtime /build/crates/eryx-runtime/runtime.cwasm crates/eryx-runtime/runtime.cwasm

# Copy full workspace source.
COPY . .

# Build the server with preinit support.
# runtime.cwasm already exists, so build.rs will embed it.
RUN cargo build --release -p eryx-server --features eryx/embedded,eryx/preinit

# Stage 5: Runtime image.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051

ENTRYPOINT ["eryx-server"]
