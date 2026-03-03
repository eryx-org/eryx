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

# Stage 3: Build workspace crates (deps already cached from cook).
FROM rust:1.90-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Bring in pre-built dependencies from cook stage.
COPY --from=cook /build/target target

# Copy full workspace source.
COPY . .

# Step 1: Extract Python stdlib for pre-initialization.
RUN mkdir -p crates/eryx-wasm-runtime/tests/python-stdlib && \
    tar -xf crates/eryx/python-stdlib.tar.zst \
      -C crates/eryx-wasm-runtime/tests/ \
      --strip-components=0

# Step 2: Build eryx-precompile and produce runtime.cwasm with preinit.
# ERYX_CPU_FEATURES controls the target arch (default: x86-64-v3 for broad compat).
ARG ERYX_CPU_FEATURES=x86-64-v3
RUN ERYX_PRECOMPILE_BOOTSTRAP=1 \
    cargo run --release -p eryx-precompile -- compile \
      crates/eryx-runtime/runtime.wasm \
      --preinit \
      --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
      --target "${ERYX_CPU_FEATURES}" \
      --output crates/eryx-runtime/runtime.cwasm \
      --no-verify

# Step 3: Build the server with preinit support.
# runtime.cwasm now exists, so build.rs will embed it.
RUN cargo build --release -p eryx-server --features eryx/embedded,eryx/preinit

# Stage 4: Runtime image.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051

ENTRYPOINT ["eryx-server"]
