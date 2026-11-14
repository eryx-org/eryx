# Upgrade notes: multi-language API rename (#820)

These notes are for the release that ships #820. Copy them into the release
PR's CHANGELOG entry by hand: release-plz doesn't generate them, and they have
to be re-applied if release-plz regenerates the PR.

eryx is getting more guest languages than Python (JavaScript, then a shell).
This release renames the public API so it doesn't assume Python. Only Python
runs today. The deprecated aliases below will be removed in the following
minor release.

## Rust (`eryx` crate)

### Renamed, with deprecated aliases

Code that uses the old names still compiles, with deprecation warnings.

| Old | New |
|---|---|
| `PythonExecutor` | `Executor` |
| `PythonStateSnapshot` (also `eryx::session::PythonStateSnapshot`) | `StateSnapshot` |
| `SandboxBuilder::with_python_stdlib` | `SandboxBuilder::with_stdlib` |
| `SandboxBuilder::with_site_packages` | `SandboxBuilder::with_library_path` |
| `Executor::with_python_stdlib` | `Executor::with_stdlib` |
| `Executor::with_site_packages` | `Executor::with_library_path` |
| `Executor::python_stdlib_path` | `Executor::stdlib_path` |
| `Executor::python_site_packages_paths` | `Executor::library_paths` |
| `StateSnapshot::skipped_globals` | `StateSnapshot::skipped` |

### Breaking changes

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

- **`SandboxBuilder` has one type parameter instead of two.** The stdlib is no
  longer tracked at compile time, because only some languages have one.
  `SandboxBuilder<state::Has, state::Has>` becomes `SandboxBuilder<state::Has>`
  (for example in `SandboxPool::with_builder` callers). Code that only chains
  builder methods is unaffected.

  `build()` is now available as soon as a runtime is configured. When no
  stdlib is set for Python, `build()` uses the embedded one (with the
  `embedded` feature), or else searches for one the way `with_auto_stdlib()`
  does, and fails with `ErrorKind::MissingStdlib` if none is found. Before,
  this was a compile error.

  `with_auto_stdlib()` and `with_embedded_stdlib()` keep their names and now
  return `Result<Self, Error>`.

- **New `ErrorKind::MissingRuntime { language }`**, returned when a language
  other than Python is selected without supplying its runtime. Code that
  matches `ErrorKind` exhaustively already needs a wildcard arm (the enum is
  `#[non_exhaustive]`).

- **State snapshots use format version 2**, which records the guest language.
  This release reads snapshots from every earlier release (they are all
  Python), but older releases can't read snapshots written by this one.
  Upgrade every process that restores snapshots before any process starts
  writing them. Restoring a snapshot into a session of another language
  fails with `ErrorKind::Snapshot`.

### New

- `Language::{Python, JavaScript, Shell}`, `SandboxBuilder::with_language`,
  `Executor::with_language` and `Executor::language`. Python is the default
  and the only language with an embedded runtime. Selecting another language
  without a runtime fails at `build()` with `ErrorKind::MissingRuntime`; a
  stdlib, packages or native extensions with a non-Python language fail with
  `ErrorKind::Initialization`.
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
