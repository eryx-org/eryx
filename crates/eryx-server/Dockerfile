FROM rust:1.90-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests for dependency caching.
COPY Cargo.toml Cargo.lock ./
COPY crates/eryx/Cargo.toml crates/eryx/Cargo.toml
COPY crates/eryx-server/Cargo.toml crates/eryx-server/Cargo.toml
COPY crates/eryx-macros/Cargo.toml crates/eryx-macros/Cargo.toml
COPY crates/eryx-runtime/Cargo.toml crates/eryx-runtime/Cargo.toml
COPY crates/eryx-vfs/Cargo.toml crates/eryx-vfs/Cargo.toml
COPY crates/eryx-wasm-runtime/Cargo.toml crates/eryx-wasm-runtime/Cargo.toml
COPY crates/wit-dylib-ffi/Cargo.toml crates/wit-dylib-ffi/Cargo.toml

# Create stub lib files so cargo can resolve the workspace.
RUN for d in crates/*/; do \
      mkdir -p "$d/src" && \
      echo "" > "$d/src/lib.rs"; \
    done && \
    mkdir -p crates/eryx-server/src && \
    echo "fn main() {}" > crates/eryx-server/src/main.rs

# Pre-build dependencies.
RUN cargo build --release -p eryx-server --features eryx/embedded 2>/dev/null || true

# Now copy the actual source code.
COPY crates/ crates/

# Build the server.
RUN cargo build --release -p eryx-server --features eryx/embedded

# Runtime image.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051

ENTRYPOINT ["eryx-server"]
