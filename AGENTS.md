# AGENTS.md - Rust & Cargo Workspace Best Practices

This document outlines best practices for AI agents (and humans) working on this Rust codebase.

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
unsafe_code = "forbid"
missing_docs = "warn"
rust_2018_idioms = "warn"

[workspace.lints.clippy]
all = "warn"
pedantic = "warn"
nursery = "warn"
unwrap_used = "warn"
expect_used = "warn"
panic = "warn"
```

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
- Use `#[allow(clippy::...)]` sparingly and with justification comments
- All clippy configuration belongs in `Cargo.toml` under `[workspace.lints.clippy]`

### Testing

- **Use `cargo nextest run --workspace`** for running tests (faster and better output than `cargo test`)
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

## Common Gotchas

1. **Don't mix workspace and non-workspace dependencies** - always use workspace inheritance
2. **Remember to add `[lints] workspace = true`** to each subcrate's `Cargo.toml`
3. **Use `--workspace` flag** for cargo commands to ensure all crates are covered
4. **Always commit `Cargo.lock`** to version control
5. **Keep lint config in `Cargo.toml`** - avoid separate clippy.toml files