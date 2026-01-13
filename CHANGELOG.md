# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## `eryx-wasm-runtime` - [0.2.0](https://github.com/eryx-org/eryx/compare/eryx-wasm-runtime-v0.1.0...eryx-wasm-runtime-v0.2.0) - 2026-01-13

### Added
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- *(eryx-wasm-runtime)* implement execution tracing
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation
- *(eryx-wasm-runtime)* implement callback infrastructure for host function access
- Implement state management exports (snapshot/restore/clear)
- *(eryx-wasm-runtime)* implement execute export with output capture
- *(eryx-wasm-runtime)* wire up Python interpreter initialization
- *(eryx-wasm-runtime)* add CPython FFI bindings module
- Add eryx-wasm-runtime crate for native sandbox exports

### Fixed
- check ERYX_PYTHON_STDLIB env var in runtime_test.rs
- rename invoke() parameter to avoid conflicts with callback kwargs
- *(docs)* escape generic type in rustdoc comment
- *(ci)* auto-decompress libs in build.rs
- resolve session state and link test failures

### Other
- Update to wasmtime 40 and crates.io wasm-tools 0.243
- Run mise unify
- move Python execution infrastructure to init time
- Fix callback return value bug and execution bench
- Rename/improve mise tasks, remove some unnecessary Python code
- Get all tests passing
- Fix rustfmt and lint errors
- Use our own async runtime
- Improve docs of componentize-py shims
- Fix another TODO
- Run rustfmt
- Remove some old docs
- update build.sh references to build.rs
- Fix clippy lints
- WIP
- Add comprehensive integration tests for Python execution
- *(eryx-wasm-runtime)* clarify wasm32-wasip1 vs wasip2 in docs

## `eryx-python` - [0.2.0](https://github.com/eryx-org/eryx/compare/eryx-python-v0.1.0...eryx-python-v0.2.0) - 2026-01-13

### Added
- add epoch-based execution timeout support
- *(eryx-python)* add PreInitializedRuntime for fast sandbox creation
- *(eryx-python)* add native extensions and package loading support
- *(eryx-python)* add Phase 1 PyO3 Python bindings (MVP)

### Other
- *(pyeryx)* rename PreInitializedRuntime to SandboxFactory and simplify Sandbox API
- Include notes on PreInitializedRuntime in Python README
- Fix links to nonexistent github repo
- Use 'pyeryx' for Python package
- Run mise unify

## `eryx` - [0.2.0](https://github.com/eryx-org/eryx/compare/eryx-v0.1.0...eryx-v0.2.0) - 2026-01-13

### Added
- add epoch-based execution timeout support
- [**breaking**] add compile-time safety to SandboxBuilder with typestate pattern
- add with_package_bytes() for loading packages from raw bytes
- improve CI with feature matrix and auto-detect stdlib
- split preinit feature from native-extensions
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- add InstancePreCache for ~8000x faster sandbox creation
- add cargo-rail and cargo-all-features support
- simplify feature flags from 6 to 2
- *(eryx-wasm-runtime)* implement execution tracing
- *(eryx)* automatic cache for native extensions
- *(eryx)* make embedded runtime the automatic default
- *(eryx)* support multiple packages and clarify embedded runtime behavior
- *(eryx)* add with_package() for easy package loading
- *(eryx)* add embedded stdlib and mmap-based runtime loading
- Add memory benchmarks and document mmap optimization
- *(eryx)* add pre-initialization support for native extensions
- *(eryx)* implement pre-compilation caching for native extensions
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation

### Fixed
- start epoch ticker after instantiation for execution timeout
- set initial epoch deadline before executing code
- use embedded stdlib in precompile verification step
- check ERYX_PYTHON_STDLIB env var in test files
- remove embedded requirement from precompile example
- update preinit vs native-extensions feature handling
- benchmark clippy lints and asyncio.run issue
- resolve clippy unnecessary_lazy_evaluations warning
- format code with cargo fmt
- *(ci)* auto-decompress libs in build.rs
- *(ci)* export WASI_SDK_PATH in mise tasks for build.rs
- resolve session state and link test failures

### Other
- Fix clippy lint
- Update to wasmtime 40 and crates.io wasm-tools 0.243
- Run cargo fmt
- invalidate cached binaries when cwasm changes
- add builder pattern for execute APIs
- add jinja2 sandbox example
- Handle stdlib in embedded python executor constructor
- Cargo fmt
- Remove broken benchmark
- simplify build system and fix stdlib extraction
- move Python execution infrastructure to init time
- Don't reference private item in doc comment
- Run cargo fmt
- Fix callback return value bug and execution bench
- Deduplicate callback handler and clean up accumulated cruft
- Use async_trait instead of RPITIT so we can have dynamically dispatched Sessions
- Make timeout tests more flexible to help with CI variability
- Set PYTHONHOME to avoid warning messages
- Actually run main doc test in lib.rs; specify all features for docs.rs
- Rename/improve mise tasks, remove some unnecessary Python code
- Maybe fix CI once and for all
- Fix rustfmt and lint errors
- Run rustfmt
- Fix clippy lints
- Tune wasmtime config for smaller footprint
- Share wasmtime Engine globally and reduce memory limit
- *(eryx)* add mmap-based cache loading for 2x faster sandbox creation
- Fix clippy lint complaint
- Fix CI issues
- Add example of streaming traces
- Add many more tests and fix a few bugs
- Use wrapper types rather than exposing schemars in our API
- Add better typing for callbacks, and add runtime/dynamic callbacks
- Improve API to use direct function calls instead of 'invoke'
- Add memory tracking; remove pooling allocator
- Support session reuse
- Add resource_limits example demonstrating ResourceLimits usage
- Add CI workflow and improve build configuration
- Fix unsafe usage; add 'precompiled' feature flag
- Add precompiled runtime feature; tidy up unsafe stuff
- Add benchmarks, more examples, and improve instantiation speed
- Add examples and actual implementation
- Second stage
- Initial commit

## `eryx-runtime` - [0.2.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.1.0...eryx-runtime-v0.2.0) - 2026-01-13

### Added
- split preinit feature from native-extensions
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- add cargo-rail and cargo-all-features support
- simplify feature flags from 6 to 2
- *(eryx)* add pre-initialization support for native extensions
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation

### Fixed
- update preinit vs native-extensions feature handling
- benchmark clippy lints and asyncio.run issue
- add build-native-extensions task for --all-features commands
- format code with cargo fmt
- *(ci)* auto-decompress libs in build.rs
- *(ci)* use POSIX-compatible test syntax in ensure-wasi-sdk
- resolve session state and link test failures

### Other
- Fix preinit stub to use 'invoke' instead of '[async]invoke'
- Deduplicate callback handler and clean up accumulated cruft
- Rename/improve mise tasks, remove some unnecessary Python code
- Maybe fix CI once and for all
- only build WASM when BUILD_ERYX_RUNTIME is set or wasm missing
- Fix rustfmt and lint errors
- Remove unused runtime files; update architecture doc
- Run rustfmt
- Fix clippy lints
- Improve API to use direct function calls instead of 'invoke'
- Support session reuse
- Add CI workflow and improve build configuration
- Add examples and actual implementation
- Second stage
