# Upgrade notes: multi-language API rename (#820)

These notes are for the release that ships #820. Copy them into the release
PR's CHANGELOG entry by hand: release-plz doesn't generate them, and they have
to be re-applied if release-plz regenerates the PR.

eryx is getting more guest languages than Python (JavaScript, then a shell).
This release renames the public API so it doesn't assume Python. Only Python
runs today. The deprecated aliases below will be removed in the following
minor release.

## Rust (`eryx` crate)

### The builder's language

`SandboxBuilder` now carries the guest language as a type parameter:
`SandboxBuilder<Lang = Python, Runtime = state::Needs>`. `Sandbox::builder()`
and `Sandbox::embedded()` return a Python builder, so code that only chains
builder methods keeps compiling unchanged.

- `.javascript()` and `.shell()` switch the language, keeping the
  language-neutral configuration (runtime source, callbacks, handlers, limits,
  network, secrets, volumes, ...).
- The Python-only options are methods of `SandboxBuilder<Python, _>` alone,
  and keep their names: `with_python_stdlib`, `with_auto_stdlib`,
  `with_embedded_stdlib`, `with_embedded_runtime`, `with_site_packages`,
  `with_package`, `with_package_bytes`, `with_native_extension`, `with_cache`
  and `with_cache_dir`.
- `build()` works for any language and returns the same untyped `Sandbox`.
  Only Python has an embedded runtime: for JavaScript or the shell, supply the
  runtime with `with_wasm_file()` or a precompiled equivalent, or `build()`
  fails with `ErrorKind::MissingRuntime`. Python-only options set before
  switching languages make `build()` fail with `ErrorKind::Initialization`.
- The marker types `Python`, `JavaScript` and `Shell` and their sealed
  `GuestLanguage` trait live in the new `eryx::language` module, next to the
  runtime `Language` enum (also re-exported as `eryx::Language`).

### Renamed, with deprecated aliases

Code that uses the old names still compiles, with deprecation warnings.

| Old | New |
|---|---|
| `PythonExecutor` | `Executor` |
| `PythonStateSnapshot` (also `eryx::session::PythonStateSnapshot`) | `StateSnapshot` |
| `Executor::with_python_stdlib` | `Executor::with_stdlib` |
| `Executor::with_site_packages` | `Executor::with_library_path` |
| `Executor::python_stdlib_path` | `Executor::stdlib_path` |
| `Executor::python_site_packages_paths` | `Executor::library_paths` |
| `StateSnapshot::skipped_globals` | `StateSnapshot::skipped` |

### Breaking changes

- **`SandboxBuilder`'s type parameters changed.** It was
  `SandboxBuilder<Runtime, Stdlib>`. It is now `SandboxBuilder<Lang, Runtime>`,
  and the stdlib is no longer tracked in the type. Code that names the type,
  for example a helper that returns a configured builder, changes like this:

  ```rust
  // Before
  fn builder() -> SandboxBuilder<state::Has, state::Has> { /* ... */ }
  // After
  fn builder() -> SandboxBuilder<eryx::language::Python, state::Has> { /* ... */ }
  ```

  `SandboxPool::new` is now generic over the language and takes any
  `SandboxBuilder<L, state::Has>`.

  `build()` is now available as soon as a runtime is configured. When no
  stdlib is set for Python, `build()` uses the embedded one (with the
  `embedded` feature), or else searches for one the way `with_auto_stdlib()`
  does, and fails with `ErrorKind::MissingStdlib` if none is found. Before,
  leaving out the stdlib was a compile error. `with_auto_stdlib()` and
  `with_embedded_stdlib()` now return `Result<Self, Error>`.

- **`ErrorKind::PythonException { traceback, stdout, stderr }` is now
  `ErrorKind::ScriptFailed { message, exit_code, stdout, stderr }`.** `message`
  holds what `traceback` did (the Python traceback). `exit_code` is the
  script's exit status for guests that have one (a shell); it is `None` for
  Python. The variant is `#[non_exhaustive]`, so match it with `..`:

  ```rust
  // Before
  if let ErrorKind::PythonException { traceback, .. } = err.kind() { /* ... */ }
  // After
  if let ErrorKind::ScriptFailed { message, .. } = err.kind() { /* ... */ }
  ```

- **`ErrorKind::MissingPythonStdlib` is now `ErrorKind::MissingStdlib { language }`.**
  Match it with `ErrorKind::MissingStdlib { .. }`.

- **New `ErrorKind::MissingRuntime { language }`**, returned when a language
  other than Python is selected without supplying its runtime. Code that
  matches `ErrorKind` already needs a wildcard arm, because the enum is
  `#[non_exhaustive]`.

- **State snapshots use format version 2**, which records the guest language.
  This release reads snapshots from every earlier release (they are all
  Python), but older releases can't read snapshots written by this one.
  Upgrade every process that restores snapshots before any process starts
  writing them. Restoring a snapshot into a session of another language
  fails with `ErrorKind::Snapshot`.

### New

- `Language::{Python, JavaScript, Shell}` (with `Display` and `FromStr`),
  `Executor::with_language` and `Executor::language`.
- `StateSnapshot::language()` and `SnapshotMetadata::language`.
- The `ERYX_STDLIB` environment variable, checked before `ERYX_PYTHON_STDLIB`.
  `ERYX_PYTHON_STDLIB` keeps working.

## WIT (`eryx:sandbox`)

For anyone driving the runtime component directly:

- `execution-options.python-tracing` is now `tracing`.
- `state-snapshot.skipped-globals` is now `skipped`.
- `execute-error` has a new field, `exit-code: option<s32>`, after `message`.

A runtime built from this release doesn't work with an older host, or the
reverse. Rebuild precompiled runtimes and saved factories with this
release's `eryx-precompile` as usual.

## gRPC server (`eryx.v1`)

- `FAILURE_KIND_SCRIPT_EXCEPTION` is renamed `FAILURE_KIND_SCRIPT_FAILED`.
  The old name stays as a deprecated alias with the same number (1), so the
  wire format doesn't change. In generated code the enum value takes the new
  name (Rust/prost: `FailureKind::ScriptFailed`). Clients that compare the
  JSON name as a string get `FAILURE_KIND_SCRIPT_FAILED` from servers built
  with the new proto.

## Python (`pyeryx`) and JavaScript (`@eryx-org/eryx`)

No API changes. `StateSnapshot.skipped_globals` (Python),
`snapshot.skippedGlobals` (JavaScript) and `PythonExceptionError` keep their
names.
