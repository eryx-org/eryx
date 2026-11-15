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
- Library preambles are Python only, and the shell can't have secrets: a
  sandbox with either fails to build (`ErrorKind::Initialization`).
  JavaScript reads secrets' placeholders from `process.env` and sends them
  with `fetch`.
- `Executor::precompile`, `precompile_file` and `precompile_with_target` now
  fail when `ERYX_CPU_FEATURES` or `ERYX_CRANELIFT_FLAGS` is set in a build
  without the `embedded` or `preinit` feature. They used to ignore the
  variable silently and compile for the build machine's CPU, so the output
  could fail to load on a machine with fewer CPU features.
- A shell script that exits non-zero fails with
  `ErrorKind::ScriptFailed { exit_code: Some(status), .. }`. External commands
  aren't available to the shell yet and exit 127.

## gRPC server (`eryx-server`)

- New `ExecuteRequest.language` (`LANGUAGE_UNSPECIFIED` is Python). A
  language the server doesn't serve is rejected with `INVALID_ARGUMENT`. So is
  a shell request with `secrets`; JavaScript gets them in `process.env`.
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
- **The published image `sd2k/eryx-server` now comes in two variants, and
  the plain tags are the bigger one.** `latest`, `X.Y.Z`, `X.Y`, `main` and
  `<sha>` are the all-languages image (about 23 MB larger than before).
  The Python-only image, the same size as the previous release's, has the
  same tags with a `-python` suffix: `X.Y.Z-python`, `X.Y-python`,
  `latest-python`, `main-python`, `<sha>-python`. Deployments that pull
  `latest` or a version tag get the all-languages image on upgrade; switch
  to the `-python` tag to keep a Python-only image.
- `EryxService::with_pool(language, pool)` and
  `with_lazy_pool(language, factory)` add a pool; `EryxService::new` and
  `with_signer` take the Python pool as before.

## JavaScript (`@eryx-org/eryx`)

- New `language` option: `"python"` (the default), `"javascript"` or
  `"shell"`; anything else throws a `TypeError`. The JavaScript and shell
  runtimes are separate files, loaded the first time a sandbox uses them, so
  a Python-only app downloads nothing extra (the npm package grows by about
  2.4 MB). Their sandboxes start with an empty filesystem, and their script
  failures reject with `PythonExceptionError`.
- **Behaviour change:** writes to WASI fds 1 and 2 are now captured per
  sandbox and appended to the result's `stdout`/`stderr` (and streamed to the
  output handler once the run ends), as the Rust host does, with the same
  1 MiB cap per stream. For Python, this means `os.write(1, ...)` and
  `sys.__stdout__`/`sys.__stderr__` output lands in the result instead of the
  console.
- **Fix:** script failures in a `timeoutMs` sandbox now reject with
  `PythonExceptionError` (with `stdout` and `stderr`), not a bare `Error`.

## Release assets and packages

- Each `eryx-vX.Y.Z` GitHub release has `runtime-python.wasm`,
  `runtime-javascript.wasm` and `runtime-shell.wasm`, for
  `eryx-precompile setup --language <lang>`, and `coreutils.wasm`, the shell's
  embedded commands. Each has a `.sha256`.
- New PyPI packages `pyeryx-runtime-javascript` and `pyeryx-runtime-shell`,
  released with pyeryx at the same version: one platform wheel per pyeryx
  platform, holding that platform's precompiled runtime.

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
