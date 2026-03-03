FROM rust:1.90-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    zstd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests for dependency caching.
COPY Cargo.toml Cargo.lock ./
COPY crates/eryx/Cargo.toml crates/eryx/Cargo.toml
COPY crates/eryx-server/Cargo.toml crates/eryx-server/Cargo.toml
COPY crates/eryx-macros/Cargo.toml crates/eryx-macros/Cargo.toml
COPY crates/eryx-precompile/Cargo.toml crates/eryx-precompile/Cargo.toml
COPY crates/eryx-runtime/Cargo.toml crates/eryx-runtime/Cargo.toml
COPY crates/eryx-linker-wasm/Cargo.toml crates/eryx-linker-wasm/Cargo.toml
COPY crates/eryx-vfs/Cargo.toml crates/eryx-vfs/Cargo.toml
COPY crates/eryx-wasm-runtime/Cargo.toml crates/eryx-wasm-runtime/Cargo.toml
COPY crates/wit-dylib-ffi/Cargo.toml crates/wit-dylib-ffi/Cargo.toml

# Create stub lib files so cargo can resolve the workspace.
RUN for d in crates/*/; do \
      mkdir -p "$d/src" && \
      echo "" > "$d/src/lib.rs"; \
    done && \
    mkdir -p crates/eryx-server/src && \
    echo "fn main() {}" > crates/eryx-server/src/main.rs && \
    mkdir -p crates/eryx-precompile/src && \
    echo "fn main() {}" > crates/eryx-precompile/src/main.rs

# Pre-build dependencies (bootstrap mode: no runtime.cwasm yet).
RUN ERYX_PRECOMPILE_BOOTSTRAP=1 \
    cargo build --release -p eryx-server -p eryx-precompile \
      --features eryx/embedded,eryx/preinit 2>/dev/null || true

# Now copy the actual source code.
COPY crates/ crates/

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

# Runtime image.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051

ENTRYPOINT ["eryx-server"]
