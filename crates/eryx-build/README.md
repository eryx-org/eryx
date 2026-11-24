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
eryx-build toolchain go [<name|path.toml>] [--dir <dir>]
                           Fetch and build the wasip3 Go fork; prints its GOROOT
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
| `go` | the wasip3 Go fork, componentized with `wasm-tools` | the fork, built by `eryx-build toolchain go` (see below) |

All lanes need `git`, `curl` and `wasm-tools` on `PATH`, and `wasmtime` when
a manifest enables `[cwasm]` or builds with the Go lane. The manifest format
is documented on the `Manifest` type in `src/manifest.rs`.

### The Go fork

The Go lane needs a WASI 0.3 port of Go: the `wasip3-0.3.0-go1.27.4-exit-code`
branch of [sd2k/go](https://github.com/sd2k/go), at the commit eryx-build pins
(`GoToolchain::default` in `src/manifest.rs`). Build it once with

```text
eryx-build toolchain go
```

which fetches the pinned commit into
`$XDG_CACHE_HOME/eryx-build/go-wasip3/<commit>` (or `~/.cache/…`), builds it
with the fork's own `make.bash` and a host Go new enough to bootstrap Go 1.27
(on `PATH`, or at `GOROOT_BOOTSTRAP`), sets `GOWASM=widearith` in its `go.env`, and prints
the GOROOT. A later run with the toolchain already built only rewrites
`go.env`. Building takes several minutes; to cache it in CI, key the cache on
the eryx-build version (or the pinned commit) and the directory it prints.

The lane uses that directory, or `ERYX_GO_WASIP3_ROOT` when set
(`--dir <dir>` builds somewhere else). Either way the toolchain must be a
checkout at the pinned commit, or the build fails. Every commit of the fork
reports the same Go version, which Go's build cache keys on, so the lane
builds with its own `GOCACHE` per commit,
`$XDG_CACHE_HOME/eryx-build/go-build/<commit>`, and a pin bump starts from a
cold cache. A manifest that needs
another commit of the fork sets all three fields of

```toml
[build.go_toolchain]
repo = "https://github.com/sd2k/go.git"
ref = "my-branch"
commit = "<40 hex digits>"
```

and `eryx-build toolchain go <manifest>` builds that commit.

### Go's own commands

`go`, `compile`, `asm` and `link` only build from their GOROOT, so a
manifest for one of them sets `std_tool`:

```toml
[source]
repo = "https://github.com/sd2k/go.git"
ref = "wasip3-0.3.0-go1.27.4-exit-code"
commit = "<the toolchain's commit>"
dir = "scratch/go-src"

[build]
package = "cmd/go"
std_tool = true
vendor_patch = "clis/patches/patch-go.sh"
```

The source is a second checkout of the fork, at the same commit as the
toolchain (`[build.go_toolchain]`, or the default pin), and loading the
manifest fails otherwise. The lane skips `go mod vendor` and runs
`vendor_patch` on that checkout. It links the toolchain's native
`pkg/tool` into the checkout and copies its `go.env` (with `GOWASM`), then
builds `package` with the checkout as `GOROOT`. Since both commits match,
those are the tools the checkout's own `make.bash` would build, so it never
runs. Linking needs a Unix host.

### WIT imports beyond WASI

A Go command that calls eryx's host interfaces, such as `go` running its
tools through `eryx:process/run`, sets `[build] wit` as the C lane does:

```toml
wit = { path = "wit/process.wit", world = "eryx:process/child@0.1.0" }
```

Name the world with its package's version: it's included from the fork's
WIT package, where an unversioned name doesn't resolve. The lane copies the fork's WIT into `<output>/wit`, adds that file (or
directory) to its `deps/`, and componentizes with a world that includes
both `[component] world` and `wit.world`. Unlike the C lane, it generates
no bindings: the Go code declares its own `//go:wasmimport` functions,
typically added by its `vendor_patch`.

The C and Rust lanes link a small start-up object, compiled with
`rustc --target wasm32-wasip2`, that moves the command into `$PWD` when it
starts, since WASI children don't otherwise start in the shell's directory.
The C lane also links `src/exit_code.c`, which exits with
`wasi:cli/exit.exit-with-code`, since wasi-libc reports only success or
failure (any non-zero status became 1). Rust's `std::process::exit` already
uses it, but an `ExitCode` returned from `main` went the same way, so the
Rust lane links `src/exit_code.rs`, which passes that on to `exit` too.

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
that talks to eryx's host interfaces through `[build] wit` (git's or go's
`eryx:process/run` bindings, for example) needs its own copy of that WIT,
such as eryx's `crates/eryx-runtime/wit/deps/process.wit`, at the version of
the eryx release it targets.

## License

eryx-build is MIT OR Apache-2.0. The tools it builds keep their own licences,
recorded in each `provenance.json`.
