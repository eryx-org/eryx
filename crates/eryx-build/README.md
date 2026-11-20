# eryx-build

Build upstream command-line tools as WASI components that
[eryx](https://github.com/eryx-org/eryx) shell sandboxes run as child
commands. Each tool is described by a TOML manifest; eryx-build fetches its
pinned source, patches and builds it through a language lane, and writes the
component with a provenance record.

```text
cargo install eryx-build

eryx-build <name>          Build clis/<name>.toml
eryx-build <path.toml>     Build a manifest at an explicit path
eryx-build --list          List the manifests in clis/ (and clis/gpl/*/)
eryx-build --check         Validate every manifest without building, refusing
                           local `path` sources
```

The published commands' manifests (`eryx:rg`, `eryx-gpl:git` and the rest)
live in [eryx-org/eryx-commands](https://github.com/eryx-org/eryx-commands);
this repository keeps only coreutils', which eryx embeds.

Run it from the root of the repository holding the manifests. Every path a
manifest names (patches, shims, a `[build] wit` world, the output dir) is
relative to that root, and sources are fetched into its `scratch/` dir.

## Lanes and toolchains

| `lang` | Builds with | Needs on the build machine |
|---|---|---|
| `rust` | `cargo build --release --target wasm32-wasip2` | `cargo` and the `wasm32-wasip2` target |
| `c` | wasi-sdk's clang: plain, CMake, autotools or make | `WASI_SDK_PATH` pointing at wasi-sdk, `wit-bindgen`, and `rustc` with the `wasm32-wasip2` target |
| `go` | the wasip3 Go fork, componentized with `wasm-tools` | the fork, at `ERYX_GO_WASIP3_ROOT` (default `scratch/go-wasip3`) |

All lanes need `git`, `curl` and `wasm-tools` on `PATH`, and `wasmtime` when
a manifest enables `[cwasm]` or builds with the Go lane. The manifest format
is documented on the `Manifest` type in `src/manifest.rs`.

The C and Rust lanes link a small start-up object, compiled with
`rustc --target wasm32-wasip2`, that moves the command into `$PWD` when it
starts, since WASI children don't otherwise start in the shell's directory.
The C lane also links `src/exit_code.c`, which exits with
`wasi:cli/exit.exit-with-code`, since wasi-libc reports only success or
failure (any non-zero status became 1). Rust's std uses it for
`std::process::exit`, but a status returned from `main` (an `ExitCode`)
still reaches the shell as 1.

Upstream sources are pinned by content: a git `commit`, or the `sha256` of an
archive. A checkout or download that doesn't match fails the build.

### Local sources

To build a command you're working on, give the manifest a local `path`
instead of a pinned upstream:

```toml
[source]
path = "../my-cli"   # relative to the manifest's directory, or absolute
```

eryx-build builds that tree in place. Nothing is fetched, and build outputs
(`target/`, Go's `vendor/`, C objects) land in the tree as in a native build.
It doesn't run patches on a tree it doesn't own: a `path` source can't have
`build.vendor_patch` or a dep `config_patch`, nor `repo`, `commit`, `sha256` or
`dir`. The Rust lane doesn't pass `--locked`, so the lockfile can change as
you go. In `provenance.json`, `source` gets `path` in place of `url`, the
checkout's `HEAD` as `commit` (`null` outside git), and `dirty`, which is false
only for a git checkout with nothing uncommitted under `path`. A build like
this isn't reproducible: `--check` refuses manifests with a `path` source, and
anything that publishes builds should refuse a `source.path` in provenance.

## Output

The manifest's `[output] dir` gets:

- `component.wasm`, and `<name>.cwasm` when `[cwasm] enabled`;
- `manifest.toml`, a copy of the manifest the build used;
- `provenance.json`:

| Field | |
|---|---|
| `format` | Layout version (1). Fields are only added within a version. |
| `eryx_build` | The eryx-build version that made the component. |
| `name`, `description`, `license`, `lang` | From the manifest. `license` is the upstream's SPDX expression. |
| `source`, `deps` | Each upstream's URL, ref, pinned commit or sha256, and licence. A local `path` source has `path` and `dirty` instead of `url` (see [Local sources](#local-sources)). |
| `patches` | The repo files the build applies, compiles or links (`build.vendor_patch`, `build.shims`, the `build.wit` file or every file in that directory, each dep's `config_patch` and `shims`), with their sha256. |
| `artifacts` | The built files, with their sha256. |
| `imports` | The WIT packages `component.wasm` imports, with versions, sorted: `wasi:cli@0.2.12`, `eryx:process@0.1.0`, `wasi:cli@0.3.0-rc-…`. This is what a host must provide to run it, and tells a WASI 0.2 command from a 0.3 one. |

## Revisions of a built command

A command published from these builds is versioned as its upstream version
plus a revision, `<upstream>-r.<n>`. Start a new revision whenever the built
`component.wasm` changes while the upstream version stays the same:

- a change to the manifest: flags, patches, shims, dependencies or their
  pins;
- an eryx-build upgrade that changes the lane's output;
- a change in `imports`, such as a new WASI version, or `eryx:process` added
  or bumped.

An eryx-build upgrade that leaves `component.wasm` byte-identical, which the
`artifacts` sha256 shows, is not a new revision. A new upstream version starts
again at `r.1`.

## Building outside the eryx repository

Manifests name files relative to the directory eryx-build runs in. A command
that talks to eryx's host interfaces through `[build] wit` (git's
`eryx:process/run` bindings, for example) needs its own copy of that WIT,
such as eryx's `crates/eryx-runtime/wit/deps/process.wit`, at the version of
the eryx release it targets.

## License

eryx-build is MIT OR Apache-2.0. The tools it builds keep their own licences,
recorded in each `provenance.json`.
