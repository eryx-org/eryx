# Upgrade notes: more than one guest runtime (#821)

These notes are for the release that ships #821. Copy them into the release
PR's CHANGELOG entry by hand: release-plz doesn't generate them, and they have
to be re-applied if release-plz regenerates the PR.

eryx now builds, precompiles and embeds a runtime per guest language: Python
(CPython), JavaScript (QuickJS) and a bash-compatible shell (brush).

## Rust (`eryx` crate)

### New

- Cargo features `python` (default), `javascript` and `shell`. With
  `embedded`, each enabled language's precompiled runtime is embedded.
  `embedded` always includes Python. Without `embedded`, a language still runs
  from a runtime you supply with `with_wasm_file()` and friends.
- `Sandbox::embedded().javascript()` and `Sandbox::embedded().shell()` run the
  embedded JavaScript and shell runtimes, with no wasm file and no stdlib.
  `with_embedded_runtime()` now works for every language.
- `Executor::from_embedded_runtime_for(language)`, and
  `eryx::embedded::{has_runtime, runtime_path}`.
- `ERYX_RUNTIME_CWASM_JAVASCRIPT` and `ERYX_RUNTIME_CWASM_SHELL` name an
  explicit `.cwasm` to embed, like `ERYX_RUNTIME_CWASM` does for Python.
- Precompiled runtimes can be stamped with their language
  (`Executor::stamp_language`, `eryx-precompile compile --language <lang>`).
  Building a sandbox for another language from a stamped runtime fails with
  `ErrorKind::Initialization` ("this precompiled runtime runs javascript, but
  the sandbox is for python"). The runtimes eryx builds and embeds are all
  stamped; unstamped files, such as custom runtimes from earlier releases,
  aren't checked.

### Changed

- **The runtime artifacts are named after their language.** `runtime.wasm` and
  `runtime.cwasm` are now `runtime-python.wasm` and `runtime-python.cwasm`,
  next to `runtime-javascript.*` and `runtime-shell.*`. This covers the
  workspace files in `crates/eryx-runtime/` and the GitHub Release assets
  (`runtime-python.wasm`, `runtime-python.wasm.sha256`, ...). Scripts that
  download `runtime.wasm` from a release need the new name.
- **`eryx-precompile setup` caches `runtime-<language>-v<version>-<arch>-<os>.cwasm`**
  (it was `runtime-v<version>-...`). Run `eryx-precompile setup` again after
  upgrading, as after every upgrade. `setup --language javascript --language
  shell` sets up those runtimes too.
- The embedded runtime's cached copy is `runtime-<language>-<version>-<hash>.cwasm`
  in the eryx cache directory. Older copies are not cleaned up automatically.
- A JavaScript or shell sandbox rejects secrets and library preambles
  (`ErrorKind::Initialization`): the preamble that sets them up is Python.
- A shell script that exits non-zero fails with
  `ErrorKind::ScriptFailed { exit_code: Some(status), .. }`. External commands
  aren't available to the shell yet and exit 127.

## gRPC server (`eryx-server`)

- New `ExecuteRequest.language` (`LANGUAGE_UNSPECIFIED` is Python). A
  language the server doesn't serve is rejected with `INVALID_ARGUMENT`.
- New `--languages` flag (`ERYX_LANGUAGES`), default `python,javascript,shell`:
  the languages the server accepts. Each gets its own pool, sized by the pool
  flags. Python's is created and prewarmed at startup, as before; the others
  on their first request, so a server that only runs Python costs no more
  than it did. Pass `--languages python` to reject the others.
  `--runtime-cwasm` and `--stdlib` still configure Python only.
- The pool metrics (`eryx_sandbox_pool_*`) have a `language` label.
- Cargo features `javascript` and `shell` (on by default). Built without
  them, the server has no JavaScript or shell code or runtimes, and a request
  for one gets `INVALID_ARGUMENT` naming the missing feature.
- The Dockerfile takes `--build-arg ERYX_LANGUAGES=python,javascript,shell`
  (the default). `ERYX_LANGUAGES=python` builds a Python-only image the size
  of the previous release's: 170,232,862 bytes against 170,212,172 for
  `sd2k/eryx-server:0.15.0`, of which this change adds 2,832. The
  all-languages image is 193,330,478 bytes (the JavaScript and shell
  runtimes add about 23 MB).
- `EryxService::with_pool(language, pool)` and
  `with_lazy_pool(language, factory)` add a pool; `EryxService::new` and
  `with_signer` take the Python pool as before.

## JavaScript (`@eryx-org/eryx`)

- New `language` option, which only accepts `"python"` so far; anything else
  throws a `TypeError`.

## Python (`pyeryx`)

- `Sandbox(language="javascript")` and `Session(language="shell")`; the
  default is `"python"`. An unknown language raises `ValueError`.
- The pyeryx wheel still only includes the Python runtime, and is the same
  size. The JavaScript and shell runtimes come from, in order: a
  `runtime_path=` argument, `ERYX_RUNTIME_CWASM_JAVASCRIPT` /
  `ERYX_RUNTIME_CWASM_SHELL`, the companion packages `pyeryx-runtime-javascript`
  and `pyeryx-runtime-shell` (`pip install pyeryx[javascript]`, `pyeryx[shell]`
  or `pyeryx[all]`), then the cache `eryx-precompile setup --language <lang>`
  writes. Without one, the sandbox raises `InitializationError` saying how to
  get it. A runtime from another eryx version or for another language fails
  with `InitializationError` too.
- `runtime_path=` also works for Python, to run a custom precompiled runtime.

## For eryx developers

- The CPython guest crate `eryx-wasm-runtime` is now `eryx-guest-python`; the
  JavaScript and shell guests are `eryx-guest-js` and `eryx-guest-shell` (a
  standalone workspace, built for wasm32-wasip2).
- mise tasks: `build-runtime-<language>` and `precompile-runtime-<language>`.
  The old `build-eryx-runtime`, `precompile-eryx-runtime` and
  `precompile-eryx-runtime-preinit` remain as aliases.
