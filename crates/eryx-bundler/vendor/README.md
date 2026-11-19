# Vendored crates

## rolldown_plugin_utils 1.3.2

From crates.io (`rolldown_plugin_utils` 1.3.2, MIT, part of
[rolldown](https://github.com/rolldown/rolldown)), with one change so rolldown
builds for wasm32-wasip2:

- `src/asset_emission.rs`: `tokio::fs::read(..).await` → `std::fs::read(..)`.
- `Cargo.toml`: the `tokio` dependency (only there for its `fs` feature, which
  tokio refuses on wasm) is removed.

Applied through `[patch.crates-io]` in `crates/eryx-bundler/Cargo.toml` only.
Drop it once rolldown reads the file with `std::fs` on wasm upstream; when
bumping rolldown, re-vendor the matching version and reapply the change.
