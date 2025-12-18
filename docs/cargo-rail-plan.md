# Cargo-Rail and Feature Testing Plan

**Status: ✅ IMPLEMENTED**

## Overview

Add two complementary tools:
1. **cargo-rail** - Graph-aware monorepo orchestration (dependency unification, dead feature pruning, affected crate detection)
2. **cargo-all-features** - Test all feature flag combinations

## Tool Purposes

### cargo-rail
- Unify dependencies to `[workspace.dependencies]`
- Prune dead/unused features
- Detect unused dependencies
- Compute MSRV from dependency graph
- Graph-aware CI (only test affected crates)

### cargo-all-features
- Build/test all feature combinations for a crate
- Ensures no feature combination breaks compilation
- Configurable via `[package.metadata.cargo-all-features]`

## Implementation Plan

### Phase 1: Add cargo-rail

1. **Add to mise.toml tools**
   ```toml
   [tools."cargo:cargo-rail"]
   version = "latest"
   binstall = true
   ```

2. **Initialize cargo-rail config**
   ```bash
   cargo rail init  # Creates .config/rail.toml
   ```

3. **Configure `.config/rail.toml`**
   ```toml
   targets = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "wasm32-wasip1"]
   
   [unify]
   pin_transitives = false
   detect_unused = true
   prune_dead_features = true
   msrv = true
   msrv_source = "max"
   
   [change-detection]
   infrastructure = [".github/**", "scripts/**", "*.sh", "mise.toml"]
   ```

4. **Add mise tasks**
   ```toml
   [tasks.unify]
   description = "Unify workspace dependencies and prune dead features"
   run = "cargo rail unify"
   
   [tasks.unify-check]
   description = "Check for dependency drift (CI)"
   run = "cargo rail unify --check"
   
   [tasks.affected]
   description = "List affected crates"
   run = "cargo rail affected"
   ```

### Phase 2: Add cargo-all-features

1. **Add to mise.toml tools**
   ```toml
   [tools."cargo:cargo-all-features"]
   version = "latest"
   binstall = true
   ```

2. **Configure in eryx crate's Cargo.toml**
   ```toml
   [package.metadata.cargo-all-features]
   # Skip combinations that require runtime to be built
   skip_feature_sets = [
       ["native-extensions"],  # Requires WASM runtime build
   ]
   # Always include these in combinations
   always_include_features = []
   # Max features to combine (we only have 2, so test all)
   max_combination_size = 2
   ```

3. **Configure in eryx-runtime crate's Cargo.toml**
   ```toml
   [package.metadata.cargo-all-features]
   # native-extensions requires build artifacts
   denylist = ["native-extensions"]
   ```

4. **Add mise tasks**
   ```toml
   [tasks.check-all-features]
   description = "Check all feature combinations"
   run = "cargo all-features check"
   
   [tasks.test-all-features]
   description = "Test all feature combinations"
   depends = ["build-eryx-runtime"]
   run = "cargo all-features test"
   ```

### Phase 3: Update CI workflow

1. Add `cargo rail unify --check` to CI
2. Consider using `cargo rail affected` for PR-based testing
3. Add `cargo all-features check` for feature matrix validation

## Feature Combinations to Test

With simplified features (2 features = 4 combinations):

| embedded | native-extensions | Notes |
|----------|-------------------|-------|
| ❌ | ❌ | Base crate, no extras |
| ✅ | ❌ | Zero-config mode |
| ❌ | ✅ | Native extensions only (requires runtime build) |
| ✅ | ✅ | Full features (requires runtime build) |

## Expected Benefits

1. **cargo-rail unify**
   - Auto-cleanup of workspace dependencies
   - Detection of unused deps/features
   - MSRV computation

2. **cargo-all-features**
   - Catch feature interaction bugs
   - Ensure all combinations compile
   - CI validation of feature matrix

## Files to Modify

1. `mise.toml` - Add tools and tasks
2. `crates/eryx/Cargo.toml` - Add cargo-all-features config
3. `crates/eryx-runtime/Cargo.toml` - Add cargo-all-features config
4. `.config/rail.toml` - New file (created by `cargo rail init`)
5. `.gitignore` - May need to ignore rail backup files

## Verification

After implementation:
- [x] `cargo rail unify --check` passes
- [x] `cargo check-all-features` passes (base features)
- [x] `mise run check-all-features` works
- [x] CI integration (added check-features, test-base, and examples jobs)