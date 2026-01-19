# AGENTS.md - Rust & Cargo Workspace Best Practices

This document outlines best practices for AI agents (and humans) working on this Rust codebase.

## Eryx-Specific Tooling

This project uses [mise](https://mise.jdx.dev/) for tooling and task management.

### Quick Start

```bash
mise install           # Install Rust, cargo-nextest
mise run setup         # Build WASM + precompile (one-time)
mise run test          # Run tests with embedded WASM (~0.1s)
mise run ci            # Run all CI checks
```

### Key mise Tasks

```bash
mise run test          # Run tests with embedded WASM (~0.1s)
mise run lint          # cargo clippy with all warnings
mise run lint-fix      # Auto-fix clippy warnings
mise run fmt           # cargo fmt
mise run build-eryx-runtime  # Build Python WASM component
mise run precompile-eryx-runtime # Pre-compile to native code
```

See `mise.toml` for all available tasks.

## Cargo Workspace Configuration

### Dependency Management

**All dependencies MUST be declared at the workspace root and inherited by subcrates.**

In the root `Cargo.toml`:

```toml
[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
thiserror = "2.0"
anyhow = "1.0"
```

In subcrate `Cargo.toml` files:

```toml
[dependencies]
serde.workspace = true
tokio.workspace = true
```

This ensures:
- Version alignment across all crates
- Single source of truth for dependency versions
- Easier dependency updates
- Prevents accidental version mismatches

### Workspace Lints

**Configure all lints (Rust and Clippy) at the workspace level in `Cargo.toml`.**

In the root `Cargo.toml`:

```toml
[workspace.lints.rust]
missing_docs = "warn"
# Use priority -1 to ensure lint groups are applied before individual lints
rust_2018_idioms = { level = "warn", priority = -1 }

[workspace.lints.clippy]
all = "warn"
unwrap_used = "warn"
expect_used = "warn"
```

**Note**: The `unsafe_code` lint is handled per-crate rather than at workspace level when some crates need conditional unsafe (e.g., for optional features like pre-compiled WASM loading).

In subcrate `Cargo.toml` files:

```toml
[lints]
workspace = true
```

**Do NOT use a separate `clippy.toml` file** - keep all lint configuration in `Cargo.toml` for a single source of truth.

### Shared Package Metadata

**Use `[workspace.package]` for common metadata.**

```toml
[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.75"
license = "MIT OR Apache-2.0"
repository = "https://github.com/org/repo"
authors = ["Your Name <you@example.com>"]
```

In subcrate `Cargo.toml`:

```toml
[package]
name = "my-subcrate"
version.workspace = true
edition.workspace = true
license.workspace = true
# ... etc
```

### Resolver Version

**Always use resolver version 2** (default for edition 2021+, but be explicit):

```toml
[workspace]
resolver = "2"
members = ["crates/*"]
```

## Code Quality

### Formatting

- Run `cargo fmt` before committing
- Use a `rustfmt.toml` for project-specific formatting rules
- Use `cargo fmt -- --check` in CI

### Clippy

- Run `cargo clippy --workspace --all-targets --all-features` regularly
- Fix or explicitly allow all clippy warnings
- **Prefer auto-fixing over allow attributes**: Use `mise run lint-fix` (or `cargo clippy --fix --allow-dirty --workspace`) to automatically fix warnings when possible
- Use `#[allow(clippy::...)]` sparingly and with justification comments - only when auto-fix isn't applicable
- All clippy configuration belongs in `Cargo.toml` under `[workspace.lints.clippy]`

### Testing

- **NEVER use `cargo test` directly** - it runs tests sequentially in debug mode and takes minutes (each test creates a full WASM Python runtime which takes 2-5s in debug)
- **Always use `mise run test`** which uses nextest (parallel execution) with embedded/precompiled WASM
- Use `#[cfg(test)]` modules for unit tests
- Place integration tests in `tests/` directories
- Run `cargo nextest run --workspace --all-features` to test all feature combinations

### Documentation

- Run `cargo doc --workspace --no-deps --open` to generate and view docs
- Use `//!` for module-level documentation
- Use `///` for item documentation
- Document all public APIs
- Include examples in doc comments

## Error Handling

### Libraries

- Use `thiserror` for defining error types
- Make errors `Send + Sync + 'static` when possible
- Implement `std::error::Error` for all error types

### Applications

- Use `anyhow` for application-level error handling
- Provide context with `.context()` or `.with_context()`

### General Rules

- **Never use `.unwrap()` in production code** - use `.expect()` with a descriptive message, or proper error handling
- **Avoid `.expect()` where possible** - prefer `?` operator with proper error types
- Use `#[track_caller]` on functions that may panic to improve error messages

## Performance

### Release Builds

Configure release profile in root `Cargo.toml`:

```toml
[profile.release]
lto = true
codegen-units = 1
panic = "abort"  # if you don't need unwinding
strip = true
```

### Benchmarking

- Use `cargo bench` with criterion or divan
- Always benchmark release builds

## Security

### Auditing

- Run `cargo audit` regularly to check for known vulnerabilities
- Run `cargo deny check` for license and security policy enforcement
- Keep dependencies updated with `cargo update`

### Best Practices

- Never hardcode secrets or API keys
- Use environment variables or secure vaults for sensitive configuration
- Minimize use of `unsafe` - require justification comments when used
- Review transitive dependencies

## Version Control

**Always commit `Cargo.lock`** for all crates (both libraries and applications). This ensures:
- Reproducible builds across all environments
- Consistent CI results
- Easier debugging of dependency-related issues

## Project Structure

Recommended workspace layout:

```
project/
├── Cargo.toml          # Workspace root with [workspace.dependencies] and [workspace.lints]
├── Cargo.lock          # Always committed
├── rustfmt.toml        # Formatting configuration
├── deny.toml           # cargo-deny configuration
├── crates/
│   ├── core/           # Core library
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── cli/            # CLI application
│   │   ├── Cargo.toml
│   │   └── src/
│   └── utils/          # Shared utilities
│       ├── Cargo.toml
│       └── src/
├── tests/              # Integration tests
└── benches/            # Benchmarks
```

## Feature Flags

- Document all feature flags in crate-level documentation
- Use `default = []` for libraries to avoid bloat
- Be explicit about feature dependencies
- Test with `--all-features` and `--no-default-features`

```toml
[features]
default = []
full = ["feature-a", "feature-b"]
feature-a = ["dep:optional-dep"]
feature-b = []
```

## CI Recommendations

Minimum CI checks:

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo doc --workspace --no-deps
cargo audit
```

For this project specifically, use:

```bash
mise run ci  # Runs fmt-check, lint, test
```

The `test` task uses the `embedded` feature with precompiled WASM which reduces test time from ~50s to ~0.1s.

## Common Gotchas

1. **Don't mix workspace and non-workspace dependencies** - always use workspace inheritance
2. **Remember to add `[lints] workspace = true`** to each subcrate's `Cargo.toml`
3. **Use `--workspace` flag** for cargo commands to ensure all crates are covered
4. **Always commit `Cargo.lock`** to version control
5. **Keep lint config in `Cargo.toml`** - avoid separate clippy.toml files

## Build Caching & Staleness Issues

This project has multiple layers of caching that can cause confusing "stale build" issues. Understanding these is critical for debugging.

### Cache Layers

| Cache | Location | Invalidation | When It Gets Stale |
|-------|----------|--------------|-------------------|
| **Cargo target/** | `target/` | Automatic (mtime) | After `git checkout`, cache restore, or clock skew |
| **mise task cache** | Internal | mtime of sources vs outputs | When cargo cache has newer timestamps than sources |
| **Embedded runtime** | `/tmp/eryx-embedded/` | Content hash in filename | Old versions accumulate; shouldn't cause staleness |
| **Python extension** | `_eryx.abi3.so` | `maturin develop` | After changing `eryx-wasm-runtime` Rust code |
| **WASM artifacts** | `crates/eryx-runtime/runtime.{wasm,cwasm}` | `mise run build-eryx-runtime` | After changing `eryx-wasm-runtime` code |
| **Late-linking artifacts** | `target/*/build/eryx-runtime-*/out/*.so.zst` | Rebuild eryx-runtime | WIT interface changes (e.g., TCP/TLS) not reflected |

### Symptoms of Stale Caches

- **"Old code still running"** - You changed Rust code but behavior didn't change
- **`SandboxFactory` behaves differently than `Sandbox`** - Factory uses preinit snapshot with old bytecode
- **Tests pass locally but fail in CI** (or vice versa) - Different cache states
- **`ModuleNotFoundError` for shim modules** - ssl/socket shims not in runtime.wasm
- **`type-checking export func` errors in preinit** - Late-linking artifacts have old WIT interface

### Diagnosing Cache Issues

```bash
# Check all cache layers for staleness
mise run check-caches

# Check individual layers
mise run check-wasm-artifacts        # WASM/CWASM vs source timestamps
mise run check-embedded-cache        # /tmp/eryx-embedded state
mise run check-python-extension      # .so vs Rust source timestamps
mise run check-cargo-timestamps      # .rlib vs source timestamps
mise run check-late-linking-cache    # OUT_DIR .so.zst vs prebuilt
```

### Recovery Commands

```bash
# Nuclear option - clean everything (includes late-linking cache and /tmp/eryx-embedded)
mise run clean-artifacts
cargo clean

# Clear just the late-linking artifact cache (for preinit type-checking errors)
mise run clean-late-linking-cache

# Rebuild from scratch
mise run setup

# For Python binding development specifically
cd crates/eryx-python
maturin develop --release
```

### When to Suspect Cache Issues

1. **After `git checkout`/`git pull`/`git rebase`** - File timestamps change
2. **After restoring CI cache** - Cached artifacts may be newer than sources
3. **When behavior doesn't match code** - Classic stale cache symptom
4. **When `SandboxFactory` differs from `Sandbox`** - Preinit snapshot is stale

### Prevention

- Use `mise run --force <task>` to bypass mtime checks
- The embedded runtime cache uses content hashes (`runtime-{version}-{hash}.cwasm`)
- CI touches all source files before building to ensure fresh builds
- When in doubt, run `mise run clean-artifacts && mise run setup`

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
