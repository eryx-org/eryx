# AGENTS.md - Eryx Project Guide

This document outlines project-specific knowledge for AI agents (and humans) working on this Rust codebase.

## What is Eryx?

Eryx is a Rust library that executes Python code in a WebAssembly sandbox with async callbacks. It embeds CPython 3.14 compiled to WASM (from [componentize-py](https://github.com/bytecodealliance/componentize-py)).

Key capabilities:
- Run untrusted Python code safely in a sandbox
- Expose Rust async functions as Python callbacks (`await get_time()`)
- Session state persistence for REPL-style usage
- TCP/TLS networking with host-controlled policies
- Pre-compiled WASM for fast startup (~16ms vs ~650ms)

## Project Structure

```
crates/
├── eryx/                  # Main library - public API, Sandbox, callbacks
│   ├── src/
│   │   ├── sandbox.rs     # Sandbox builder and execution
│   │   ├── callback.rs    # TypedCallback, DynamicCallback traits
│   │   ├── session/       # State persistence (InProcessSession)
│   │   └── wasm.rs        # Wasmtime integration
│   ├── examples/          # Usage examples
│   └── benches/           # Criterion benchmarks
├── eryx-runtime/          # WASM runtime packaging
│   ├── runtime-<language>.wasm   # Built WASM components (python, javascript, shell)
│   ├── runtime-<language>.cwasm  # Pre-compiled native code
│   ├── wit/runtime.wit    # WIT interface definition
│   └── libs/              # WASI libraries (zstd compressed)
├── eryx-guest-python/     # CPython guest, compiled TO WASM
│   └── src/lib.rs         # WIT exports, Python FFI
├── eryx-guest-js/         # QuickJS guest, built the same way as CPython's
├── eryx-guest-shell/      # brush (bash) guest: a plain wasip3 component
│                          # (plus a wasip2 build for js/eryx). Its own
│                          # workspace, excluded from the root one: it only
│                          # builds for wasm and pins brush from
│                          # git, which shouldn't enter the root Cargo.lock or
│                          # host `clippy --workspace` builds. CI lints it in
│                          # the `shell-guest-lint` job.
├── eryx-python/           # Python bindings (PyO3/maturin)
├── eryx-precompile/       # CLI tool for AOT compilation
└── eryx-vfs/              # Virtual filesystem for WASM
```

## Tooling

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
mise run build-runtime-python  # Build Python WASM component
mise run precompile-runtime-python # Pre-compile with preinit snapshot
mise run precompile-runtime-javascript  # Build + pre-compile the JS runtime
mise run precompile-runtime-shell       # Build + pre-compile the shell runtime
```

See `mise.toml` for all available tasks.

## WASM Runtime Build Pipeline

Eryx embeds a WASM runtime per guest language for fast startup: Python's
always, JavaScript's and the shell's with the `javascript` and `shell` cargo
features. Each runtime has its own `build-runtime-<language>` and
`precompile-runtime-<language>` mise tasks, and changing one guest doesn't
rebuild the others. The stages below are Python's; JavaScript goes through the
same wit-dylib linking (`BUILD_ERYX_JS_RUNTIME`) without preinit or late
linking, and the shell is a plain `cargo build --target wasm32-wasip3`
(`build-runtime-shell-p2` builds the WASI 0.2 copy js/eryx uses).
Understanding this pipeline is essential:

### Build Stages

1. **Build WASM** (`mise run build-runtime-python`)
   - Compiles `eryx-guest-python` to WebAssembly
   - Outputs: `crates/eryx-runtime/runtime-python.wasm`

2. **Build Late-Linking Artifacts** (`mise run build-preinit`)
   - Builds native shared libraries for host functions (networking, etc.)
   - Outputs: `liberyx_runtime.so.zst` and related artifacts
   - Required for preinit to link against host capabilities

3. **Precompile with Preinit** (`mise run precompile-runtime-python`)
   - AOT compiles WASM to native code
   - Creates a preinitialized snapshot (Python interpreter already started)
   - Outputs: `crates/eryx-runtime/runtime-python.cwasm`

4. **Embed at Compile Time**
   - `runtime-python.cwasm` is included via `include_bytes!()` in Rust code
   - Tests and binaries load this precompiled + preinitialized runtime
   - Result: ~0.1s startup vs ~5s without precompilation

### Why This Matters

- **Without precompilation**: Each test creates a fresh Python runtime (~2-5s in debug)
- **With precompilation**: Tests share the embedded preinitialized runtime (~0.1s total)
- **`SandboxFactory` vs `Sandbox`**: Factory uses the preinit snapshot; if it's stale, behavior differs

## Testing

- **NEVER use `cargo test` directly** - it runs tests sequentially in debug mode and takes minutes
- **Always use `mise run test`** which uses nextest (parallel execution) with embedded/precompiled WASM
- For CI, use `mise run ci` which runs fmt-check, lint, and test

## Common Gotchas

1. **Don't mix workspace and non-workspace dependencies** - always use workspace inheritance
2. **Remember to add `[lints] workspace = true`** to each subcrate's `Cargo.toml`
3. **Use `--workspace` flag** for cargo commands to ensure all crates are covered
4. **Always commit `Cargo.lock`** to version control
5. **Keep lint config in `Cargo.toml`** - avoid separate clippy.toml files
6. **Third-party notices** (#837) - a new Rust dependency needs a licence `about.toml` accepts; a new licence there is a decision for review, not a fix to make CI pass. Vendored code that isn't a crate in a lock file (a C library in `crates/eryx-runtime/libs/`, an npm package in the JS guest's `src/node/`, a CLI) needs an entry in `third_party/manual.toml` with its licence files. `mise run notices-check` is what CI runs

## Build Caching & Staleness Issues

This project has multiple layers of caching that can cause confusing "stale build" issues. Understanding these is critical for debugging.

### Cache Layers

| Cache | Location | Invalidation | When It Gets Stale |
|-------|----------|--------------|-------------------|
| **Cargo target/** | `target/` | Automatic (mtime) | After `git checkout`, cache restore, or clock skew |
| **mise task cache** | Internal | mtime of sources vs outputs | When cargo cache has newer timestamps than sources |
| **Embedded runtime** | `~/.cache/eryx/` (`$XDG_CACHE_HOME/eryx`, else `$TMPDIR/eryx-<uid>`) | Content hash in filename | Old versions accumulate; shouldn't cause staleness |
| **Python extension** | `_eryx.abi3.so` | `maturin develop` | After changing `eryx-guest-python` Rust code |
| **WASM artifacts** | `crates/eryx-runtime/runtime-<language>.{wasm,cwasm}` | `mise run precompile-runtime-<language>` | After changing that language's `eryx-guest-*` code |
| **Late-linking artifacts** | `target/*/build/eryx-runtime*/out/*.so.zst` | Rebuild eryx-runtime | WIT interface changes (e.g., TCP/TLS) not reflected |

### Symptoms of Stale Caches

- **"Old code still running"** - You changed Rust code but behavior didn't change
- **`SandboxFactory` behaves differently than `Sandbox`** - Factory uses preinit snapshot with old bytecode
- **Tests pass locally but fail in CI** (or vice versa) - Different cache states
- **`ModuleNotFoundError` for shim modules** - ssl/socket shims not in runtime-python.wasm
- **`type-checking export func` errors in preinit** - Late-linking artifacts have old WIT interface

### Diagnosing Cache Issues

```bash
# Check all cache layers for staleness
mise run check-caches

# Check individual layers
mise run check-wasm-artifacts        # WASM/CWASM vs source timestamps
mise run check-embedded-cache        # ~/.cache/eryx state
mise run check-python-extension      # .so vs Rust source timestamps
mise run check-cargo-timestamps      # .rlib vs source timestamps
mise run check-late-linking-cache    # OUT_DIR .so.zst vs prebuilt
```

### Recovery Commands

```bash
# Nuclear option - clean everything (includes late-linking cache and ~/.cache/eryx)
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
