# syntax=docker/dockerfile:1
#
# Build arg: "default" precompiles the runtime-<language>.wasm components in
# crates/eryx-runtime/ (python, javascript and shell), "custom" uses pre-built
# .cwasm files. In CI, use RUNTIME_SOURCE=custom with the build-runtime-* artifacts.
ARG RUNTIME_SOURCE=default
# The guest languages to build in, comma-separated. Python is always
# included; with only `python` the image is Python-only, without the
# JavaScript and shell runtimes or their code.
ARG ERYX_LANGUAGES=python,javascript,shell

# ---------- Stage 1: Precompile WASM runtime (only when RUNTIME_SOURCE=default) ----------
FROM rust:1.100-bookworm AS precompile

RUN apt-get update && apt-get install -y --no-install-recommends \
  protobuf-compiler \
  zstd \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./

# Copy all workspace crates that affect precompilation.
COPY crates/eryx/ crates/eryx/
COPY crates/eryx-precompile/ crates/eryx-precompile/
COPY crates/eryx-runtime/ crates/eryx-runtime/
COPY crates/eryx-guest-python/ crates/eryx-guest-python/
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
RUN mkdir -p crates/eryx-guest-python/tests/python-stdlib && \
  tar -xf crates/eryx/python-stdlib.tar.zst \
  -C crates/eryx-guest-python/tests/ \
  --strip-components=0

# Build eryx-precompile and produce runtime-python.cwasm with preinit.
# The x86-64-vN levels are x86-64 only; building this image on an arm64
# host needs --build-arg ERYX_CPU_FEATURES=baseline, which pins the target
# without enabling optional features (aarch64 has no psABI presets).
ARG ERYX_CPU_FEATURES=x86-64-v3
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/build/target \
  ERYX_PRECOMPILE_BOOTSTRAP=1 \
  cargo run --release -p eryx-precompile -- compile \
  crates/eryx-runtime/runtime-python.wasm \
  --language python \
  --preinit \
  --stdlib crates/eryx-guest-python/tests/python-stdlib \
  --target "${ERYX_CPU_FEATURES}" \
  --output crates/eryx-runtime/runtime-python.cwasm \
  --no-verify

# The JavaScript and shell runtimes have no preinit.
ARG ERYX_LANGUAGES
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/build/target \
  for lang in $(echo "$ERYX_LANGUAGES" | tr , ' '); do \
  [ "$lang" = python ] && continue; \
  ERYX_PRECOMPILE_BOOTSTRAP=1 \
  cargo run --release -p eryx-precompile -- compile \
  "crates/eryx-runtime/runtime-$lang.wasm" \
  --language "$lang" \
  --target "${ERYX_CPU_FEATURES}" \
  --output "crates/eryx-runtime/runtime-$lang.cwasm" \
  --no-verify || exit 1; \
  done

# Runtime provider: default (from precompile stage output)
FROM precompile AS runtime-default

# Runtime provider: custom (from build context file)
FROM scratch AS runtime-custom
# The JavaScript and shell runtimes come with the source copy below, from
# crates/eryx-runtime/runtime-{javascript,shell}.cwasm in the build context.
ARG RUNTIME_CWASM=runtime-python.cwasm
COPY ${RUNTIME_CWASM} /build/crates/eryx-runtime/runtime-python.cwasm

# Select runtime based on RUNTIME_SOURCE arg
FROM runtime-${RUNTIME_SOURCE} AS runtime

# ---------- Stage 2: Build the server binary ----------
FROM rust:1.100-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
  protobuf-compiler \
  zstd \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy full workspace source.
COPY . .

# Copy the runtimes from the selected runtime stage AFTER the source copy so
# they aren't overwritten by any that exist in the source tree.
COPY --from=runtime /build/crates/eryx-runtime/runtime-*.cwasm crates/eryx-runtime/

# Build the server with preinit support using BuildKit cache mounts.
# - cargo registry cache: avoids re-downloading crates
# - target dir cache: enables incremental compilation across builds
# We copy the final binary out of the cache mount since it disappears after RUN.
# Only the languages in ERYX_LANGUAGES are compiled in (cargo features
# `javascript` and `shell`), so a Python-only image embeds no other runtime.
ARG ERYX_LANGUAGES
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/build/target \
  features=$(echo "$ERYX_LANGUAGES" | tr , '\n' | grep -v '^python$' | sed 's|^|eryx-server/|' | paste -sd, -) \
  && cargo build --release -p eryx-server --no-default-features \
  --features "eryx/embedded,eryx/preinit${features:+,$features}" \
  && cp target/release/eryx-server /usr/local/bin/eryx-server

# ---------- Stage 3: Runtime image ----------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# Run as a dedicated unprivileged user. The numeric UID lets Kubernetes'
# `runAsNonRoot` verify it without looking up the name. Both ports are above
# 1024, so no capabilities are needed. The embedded runtime is extracted to
# the system temp dir, which stays writable for this user.
RUN groupadd --system --gid 10001 eryx \
  && useradd --system --uid 10001 --gid eryx --no-create-home --shell /usr/sbin/nologin eryx

COPY --from=builder /usr/local/bin/eryx-server /usr/local/bin/eryx-server

ENV ERYX_LISTEN_ADDR="[::]:50051"

EXPOSE 50051 9090

USER 10001:10001

ENTRYPOINT ["eryx-server"]
