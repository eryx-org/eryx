# Feature Simplification Plan

**Status: ✅ IMPLEMENTED**

This document outlines the plan to simplify the feature flags in the `eryx` crate.

## Current State (6 features)

### `eryx` crate

| Feature | Implies | Dependencies | Description |
|---------|---------|--------------|-------------|
| `precompiled` | — | — | Enable loading pre-compiled WASM (enables unsafe code paths) |
| `embedded-runtime` | `precompiled` | `tempfile`, `wasmtime` (build) | Embed pre-compiled runtime in binary |
| `embedded-stdlib` | — | `zstd`, `tar`, `tempfile` | Embed Python stdlib (~2MB compressed) |
| `packages` | — | `zip`, `flate2`, `tar`, `tempfile`, `walkdir` | Support for `.whl` and `.tar.gz` packages |
| `native-extensions` | — | `eryx-runtime/late-linking` | Late-link native extensions (e.g., numpy) |
| `pre-init` | `native-extensions` | `eryx-runtime/pre-init` | Capture Python's initialized memory state |

### `eryx-runtime` crate

| Feature | Implies | Dependencies | Description |
|---------|---------|--------------|-------------|
| `late-linking` | — | `wit-component`, `zstd`, `sha2`, `zip` | Link native extensions into WASM component |
| `pre-init` | `late-linking` | `wasmtime-wizer`, `wasmtime`, `wasmtime-wasi`, `async-trait`, `futures`, `tempfile`, `anyhow`, `tracing` | Pre-initialization memory capture |

## Problems

1. **Too many combinations**: 6 features = 64 possible combinations, most nonsensical or untested
2. **`precompiled` is awkward**: Almost always used via `embedded-runtime`, direct use requires `unsafe`
3. **`embedded-runtime` and `embedded-stdlib` always used together**: Same purpose (zero-config sandbox)
4. **`native-extensions` and `embedded-runtime` conflict at runtime**: Code logs warning when both used
5. **`pre-init` only useful with `native-extensions`**: The separation adds mental overhead

## Proposed State (2 features)

### `eryx` crate

| Feature | Description |
|---------|-------------|
| `embedded` | Zero-config sandboxes: embedded pre-compiled runtime + stdlib |
| `native-extensions` | Native extension support with pre-init capability |

### `eryx-runtime` crate

| Feature | Description |
|---------|-------------|
| `native-extensions` | Late-linking + pre-init support (rename from `late-linking` + `pre-init`) |

**Package support becomes always-on** (no feature flag).

## Migration Mapping

| Old Feature(s) | New Feature |
|----------------|-------------|
| `precompiled` | `embedded` (or remove if only using for manual precompilation) |
| `embedded-runtime` | `embedded` |
| `embedded-stdlib` | `embedded` |
| `embedded-runtime` + `embedded-stdlib` | `embedded` |
| `packages` | (always enabled) |
| `native-extensions` | `native-extensions` |
| `pre-init` | `native-extensions` |
| `native-extensions` + `pre-init` | `native-extensions` |

## Implementation Plan

### Phase 1: Update `eryx-runtime` crate

**File: `crates/eryx-runtime/Cargo.toml`**

1. Rename `late-linking` to `native-extensions`
2. Merge `pre-init` into `native-extensions` (combine all deps)
3. Remove `pre-init` as a separate feature

**File: `crates/eryx-runtime/src/lib.rs`**

1. Change `#[cfg(feature = "late-linking")]` → `#[cfg(feature = "native-extensions")]`
2. Change `#[cfg(feature = "pre-init")]` → `#[cfg(feature = "native-extensions")]`

### Phase 2: Update `eryx` crate Cargo.toml

**File: `crates/eryx/Cargo.toml`**

1. Remove features: `precompiled`, `embedded-runtime`, `embedded-stdlib`, `packages`, `pre-init`
2. Add feature: `embedded` with combined deps from old `embedded-runtime` + `embedded-stdlib`
3. Update `native-extensions` to use `eryx-runtime/native-extensions` (renamed)
4. Move `packages` deps to regular (non-optional) dependencies
5. Update example `required-features` annotations

**New features section:**
```toml
[features]
default = []
# Zero-config sandboxes: pre-compiled runtime + stdlib embedded in binary.
# Enables unsafe code paths for wasmtime's pre-compiled component loading.
embedded = ["dep:zstd", "dep:tar", "dep:tempfile", "dep:wasmtime"]
# Native Python extension support (numpy, etc.) via late-linking.
# Includes pre-initialization for faster startup.
native-extensions = ["dep:eryx-runtime", "eryx-runtime/native-extensions"]
```

**Dependencies changes:**
- `zstd`, `tar`, `tempfile` stay optional (for `embedded`)
- `zip`, `flate2`, `walkdir` become non-optional (packages always on)
- `wasmtime` build-dep stays optional (for `embedded`)

### Phase 3: Update `eryx` crate source files

**File: `crates/eryx/src/lib.rs`**

1. Change lint cfg:
   - `#![cfg_attr(not(feature = "precompiled"), forbid(unsafe_code))]` → `#![cfg_attr(not(feature = "embedded"), forbid(unsafe_code))]`
   - `#![cfg_attr(feature = "precompiled", deny(unsafe_code))]` → `#![cfg_attr(feature = "embedded"), deny(unsafe_code))]`
2. Change module cfg:
   - `#[cfg(any(feature = "embedded-stdlib", feature = "embedded-runtime"))]` → `#[cfg(feature = "embedded")]`
   - Remove `#[cfg(feature = "packages")]` from `pub mod package` (always enabled)
   - `#[cfg(feature = "pre-init")]` → `#[cfg(feature = "native-extensions")]`

**File: `crates/eryx/src/sandbox.rs`**

1. Replace all `#[cfg(feature = "precompiled")]` with `#[cfg(feature = "embedded")]`
2. Replace all `#[cfg(feature = "embedded-runtime")]` with `#[cfg(feature = "embedded")]`
3. Replace all `#[cfg(feature = "embedded-stdlib")]` with `#[cfg(feature = "embedded")]`
4. Remove all `#[cfg(feature = "packages")]` (always enabled)
5. Keep `#[cfg(feature = "native-extensions")]` as-is

**File: `crates/eryx/src/embedded.rs`**

1. Replace all `#[cfg(feature = "embedded-runtime")]` with `#[cfg(feature = "embedded")]`
2. Replace all `#[cfg(feature = "embedded-stdlib")]` with `#[cfg(feature = "embedded")]`
3. Since both are now unified, simplify struct to always have both fields

**File: `crates/eryx/src/wasm.rs`**

1. Replace all `#[cfg(feature = "precompiled")]` with `#[cfg(feature = "embedded")]`

**File: `crates/eryx/src/cache.rs`**

1. Replace `#[cfg(feature = "native-extensions")]` - keep as-is
2. Replace `#[cfg(feature = "precompiled")]` with `#[cfg(feature = "embedded")]` where it appears in combination with native-extensions

**File: `crates/eryx/build.rs`**

1. Replace `#[cfg(feature = "embedded-runtime")]` with `#[cfg(feature = "embedded")]`

### Phase 4: Update tests

**Files: `crates/eryx/tests/*.rs`**

1. Replace `#[cfg(feature = "embedded-runtime")]` with `#[cfg(feature = "embedded")]`
2. Replace `#[cfg(feature = "embedded-stdlib")]` with `#[cfg(feature = "embedded")]`
3. Replace `#[cfg(feature = "precompiled")]` with `#[cfg(feature = "embedded")]`
4. Remove `#[cfg(feature = "packages")]` (always enabled)
5. Replace `#[cfg(feature = "pre-init")]` with `#[cfg(feature = "native-extensions")]`

### Phase 5: Update examples

**File: `crates/eryx/Cargo.toml` (example annotations)**

Update `required-features` for examples:
- `embedded_runtime` → `required-features = ["embedded"]`
- `precompile` → `required-features = ["embedded"]`
- `trace_events` → `required-features = ["embedded"]`
- `numpy_native` → `required-features = ["native-extensions"]`
- `numpy_preinit` → `required-features = ["native-extensions", "embedded"]`
- `session_bench` → `required-features = ["native-extensions", "embedded"]`
- `package_loading` → `required-features = ["native-extensions", "embedded"]`

**Example source files:**

Update any `#[cfg(feature = "...")]` in example code to match new feature names.

### Phase 6: Update documentation

**File: `crates/eryx/README.md`**

Update any feature flag references (if present).

**File: `AGENTS.md`**

Update mise tasks if they reference old feature names.

**File: `mise.toml`**

Update any task commands that use old feature names.

### Phase 7: Update CI/mise tasks

Check `mise.toml` for tasks that specify features and update them:
- `test` task features
- `lint` task features
- Any other tasks

## Verification Checklist

After implementation:

- [x] `cargo build` works with no features
- [x] `cargo build --features embedded` works
- [x] `cargo build --features native-extensions` works (requires runtime build)
- [x] `cargo build --all-features` works (requires runtime build)
- [ ] `mise run test` passes
- [ ] `mise run lint` passes
- [x] All examples compile with their required features
- [x] No warnings about unused cfg conditions

## Rollback Plan

If issues arise, the changes can be reverted by:
1. Restoring old `Cargo.toml` files from git
2. Restoring old source files from git

The changes are purely additive/renamings with no data migrations needed.