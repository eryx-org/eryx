# Building Commands

A shell sandbox runs WASI command programs as external commands. This page
takes a CLI from source to a command a shell sandbox runs: building it for
WASI, checking it, registering it from Rust, Python or eryx-server, and
publishing it. [External Commands](./sandboxes.md#external-commands) is the
reference for what a running command gets.

In short:

1. Build the CLI as a WASI command: `wasm32-wasip2` for Rust, wasi-sdk for C.
2. Check it with `eryx-precompile commands inspect`.
3. Register it on a shell sandbox by name.
4. Publish the `.wasm`, and have hosts pin it by its sha256 digest.

[eryx-build](#building-with-eryx-build) does step 1 from a manifest and
fixes the start-up differences below. eryx's own published commands (ripgrep,
jq, curl, git, gh and the rest) are built with it.

## A Rust command

A small `wc`-like CLI, `count`:

```rust,ignore
//! `count`: lines, words and bytes of stdin or of each file named.
use std::io::Read;

fn count(name: &str, mut input: impl Read) -> std::io::Result<()> {
    let mut text = Vec::new();
    input.read_to_end(&mut text)?;
    let lines = text.iter().filter(|&&b| b == b'\n').count();
    let words = text.split(u8::is_ascii_whitespace).filter(|w| !w.is_empty()).count();
    println!("{lines} {words} {} {name}", text.len());
    Ok(())
}

fn main() {
    // WASI programs start in `/`: move to the shell's directory, which
    // eryx passes in $PWD.
    if let Ok(pwd) = std::env::var("PWD") {
        let _ = std::env::set_current_dir(pwd);
    }
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        if count("-", std::io::stdin()).is_err() {
            std::process::exit(1);
        }
        return;
    }
    let mut status = 0;
    for file in &files {
        if let Err(e) = std::fs::File::open(file).and_then(|f| count(file, f)) {
            eprintln!("count: {file}: {e}");
            status = 2;
        }
    }
    // Not `ExitCode` from `main`: on wasm32-wasip2 that reports only 0 or 1.
    std::process::exit(status);
}
```

Build it as a WASI 0.2 component. Rust's `wasm32-wasip2` target writes one
directly:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
eryx-precompile commands inspect target/wasm32-wasip2/release/count.wasm
```

`commands inspect` (from `cargo binstall eryx-precompile`) lists what the
command imports and whether a shell's children get each:

```text
target/wasm32-wasip2/release/count.wasm: WASI 0.2 command
imports:
  wasi:cli/environment@0.2.12: provided
  wasi:cli/exit@0.2.12: provided
  ...
  wasi:filesystem/types@0.2.12: provided
  wasi:filesystem/preopens@0.2.12: provided
exports:
  wasi:cli/run@0.2.12
eryx can run it as a shell command
```

Register it on a shell sandbox. This needs eryx's `shell` feature, and with
`embedded` the shell runtime set up once with
`eryx-precompile setup --language shell` (see
[Pre-compiling Runtimes](./precompile.md#setup-options)):

```toml
[dependencies]
eryx = { version = "0.16", features = ["embedded", "shell"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust,ignore
use eryx::{Command, Sandbox};

let sandbox = Sandbox::embedded()
    .shell()
    .with_command("count", Command::from_file("count.wasm")?)
    .build()?;

let result = sandbox
    .execute("printf 'one two\\nthree\\n' | count")
    .await?;
assert_eq!(result.stdout_text(), "2 3 14 -\n");

// A failing script is an error carrying its exit status.
let err = sandbox.execute("count /nope").await.unwrap_err();
assert!(matches!(err.kind(), eryx::ErrorKind::ScriptFailed { exit_code: Some(2), .. }));
```

`Command::from_file` compiles the command once; register the same `Command`
under several names for a multicall binary, which picks its tool from
`argv[0]`.

### Exit status

The shell sees a command's exit status as `$?`, and a script's last one is
the execution's. Two things in common toolchains lose it:

- **Rust:** returning an `ExitCode` from `main` on
  `wasm32-wasip2` or `wasm32-wasip3` reports only success or failure, so any
  non-zero status becomes 1, with or without eryx-build (a fix in eryx-build
  is tracked in [#1131](https://github.com/eryx-org/eryx/issues/1131)).
  `std::process::exit(n)` keeps `n`.
- **C with plain wasi-sdk:** wasi-libc's `exit` reports only success or
  failure too. eryx-build's C lane fixes this (below).

A WASI preview 1 module (`wasm32-wasip1`) reports only 0 or 1 whatever it
does, because eryx adapts it into a component when it loads it.

### Starting directory

A command gets the shell's directory as `PWD`, but WASI has no working
directory of its own, so a plain wasi-libc program (Rust, C) starts in `/`
(WebAssembly/wasi-libc#915): `cd /tmp/work && count notes.txt` looks for
`/notes.txt`. Either move to `PWD` at start-up, as `count` does with
`std::env::set_current_dir`, or build with eryx-build, which links a
constructor that does it for you. Absolute paths work either way.

## Building with eryx-build

eryx-build builds a command from a TOML manifest that pins its source. On
top of a plain build, it:

- links a start-up constructor that moves the command into `$PWD` (Rust and
  C);
- in the C lane, links a shim that exits with
  `wasi:cli/exit.exit-with-code`, so statuses above 1 reach the shell;
- writes `provenance.json` next to the component: the source pins, patches,
  the eryx-build version, the built file's sha256, and the WIT packages it
  imports;
- drives the C (wasi-sdk: single files, CMake, autotools or make) and Go
  toolchains, with `[[deps]]` for libraries built before the main project.

It doesn't write third-party notices; produce those with your usual tools
(such as `cargo about`).

It isn't on crates.io yet. Install it from an eryx release tag:

```bash
cargo install --locked --git https://github.com/eryx-org/eryx --tag eryx-v0.16.0 eryx-build
```

A manifest for `count`, as `clis/count.toml`:

```toml
name = "count"
description = "count: lines, words and bytes"
license = "MIT"
lang = "rust"

[source]
repo = "https://github.com/you/count.git"
ref = "v0.1.0"
commit = "<40-digit commit hash>"
dir = "scratch/count"

[build]
bin = "count"

[output]
dir = "out/count"
```

```bash
eryx-build count     # or: eryx-build clis/count.toml
```

Run it from the directory holding `clis/`: every path in a manifest is
relative to it. A git source must be pinned by `commit` (an archive URL by
`sha256` instead), and the Rust lane builds with `--locked`, so commit a
`Cargo.lock`. A `file://…/.git` URL works for building a local repository.
The component lands in `out/count/component.wasm`, next to `provenance.json`.

The [eryx-build README](https://github.com/eryx-org/eryx/tree/main/crates/eryx-build)
covers the lanes, what each needs installed, the output, and how to number
revisions of a built command; the `Manifest` type in its `src/manifest.rs`
documents every field.

## C commands

wasi-sdk's clang builds a WASI 0.2 command directly:

```bash
$WASI_SDK_PATH/bin/clang --target=wasm32-wasip2 -O2 -o hello.wasm hello.c
```

Such a program starts in `/` and reports any non-zero status as 1. Build it
with eryx-build's C lane instead to get the working directory and exit
status right:

```toml
name = "hello"
description = "hello (C)"
license = "MIT"
lang = "c"

[source]
repo = "https://github.com/you/hello.git"
ref = "v0.1.0"
commit = "<40-digit commit hash>"
dir = "scratch/hello"

[build]
# `system` defaults to "single": one clang call over `sources`.
sources = ["hello.c"]
cflags = ["-O2"]

[output]
dir = "out/hello"
```

```bash
WASI_SDK_PATH=/path/to/wasi-sdk eryx-build hello
```

The C lane also needs `wit-bindgen` and `rustc` with the `wasm32-wasip2`
target, for the shims. For CMake, autotools and make projects, set
`[build] system` and `artifact`; libraries such as oniguruma (for jq) or
mbedTLS (for curl) go in `[[deps]]`. Projects often need wasi-sdk's
emulation libraries (`-D_WASI_EMULATED_SIGNAL` with
`-lwasi-emulated-signal`, and the like) or a `vendor_patch` script that
stubs out what WASI lacks.

## Go commands

Go is experimental. Upstream Go's `GOOS=wasip1` builds preview 1 modules,
which eryx adapts but which report only 0 or 1 as their status. eryx-build's
Go lane instead builds WASI 0.3 commands with a fork of Go ported to WASI
0.3 (as implemented by the wasmtime eryx uses), and componentizes them with
`wasm-tools`. The fork is the `wasip3-0.3.0-go1.27.4-exit-code` branch of
[sd2k/go](https://github.com/sd2k/go/tree/wasip3-0.3.0-go1.27.4-exit-code)
(commit `331bf58b`): build it with its `src/make.bash` and point
`ERYX_GO_WASIP3_ROOT` at the checkout. eryx-build bootstrapping the fork
itself is coming in [#1132](https://github.com/eryx-org/eryx/issues/1132).
A manifest's Go-specific parts:

```toml
lang = "go"

[build]
package = "./cmd/tool"

[component]
world = "command"
```

eryx's `gh` is built this way. Commands built with that branch start in
the shell's directory and keep their `os.Exit` status; earlier builds of the
fork report any non-zero status as 1. The fork's version string doesn't
change between commits, so build with a fresh `GOCACHE` after moving it.

## When a command won't load

`Command::from_file` (and pyeryx's `Command.from_file`, eryx-server's
`--shell-command`, `eryx-precompile compile`) refuses a command that imports
what children don't get, naming each import. `commands inspect` shows the
same before you register it, and exits 1:

```text
httpcli.wasm: WASI 0.2 command
imports:
  wasi:cli/environment@0.2.12: provided
  ...
  wasi:http/types@0.2.12: not provided: children get no wasi:http; they reach the network over sockets, through the sandbox's proxy (HTTPS_PROXY), when it gives them network
exports:
  wasi:cli/run@0.2.12
eryx can't run it: it imports what children don't get (above)
```

- **`wasi:http`:** children have no `wasi:http`. Use an HTTP client that
  works over `wasi:sockets` (Rust's sockets, or libcurl in C) and honours
  `HTTPS_PROXY`; with network, the sandbox's proxy carries its requests.
- **WASI prereleases** (`0.2.0-rc-…`, `0.3.0-rc-…`): not provided; rebuild
  with a toolchain that targets a released WASI version.
- **`eryx:process/run` or `pipe`:** provided, but only to commands
  registered with `allow_spawn` (see
  [below](#commands-that-run-commands)).
- **A preview 1 module:** `commands inspect` says it adapts it, and lists
  what the adapted component imports. It runs, but reports only 0 or 1 as
  its status.

## Registering elsewhere

- **Python (pyeryx):**

  ```python
  import eryx

  count = eryx.Command.from_file("count.wasm")
  sandbox = eryx.Sandbox(language="shell", commands={"count": count})
  print(sandbox.execute("printf 'one two\\nthree\\n' | count").stdout_text)
  ```

  A script that fails raises `eryx.PythonExceptionError`, whose `exit_code`
  is the status. `Command.from_bytes` and `Command.from_precompiled_file`
  load one from bytes or a precompiled file; `allow_tunnels=True` lets it
  open blind tunnels through the proxy.
- **eryx-server:** `--shell-command count=/commands/count.wasm@sha256:<hex>`
  (repeatable, or `ERYX_SHELL_COMMANDS`), with `--languages python,shell`.
  A pinned file that doesn't match fails startup, and the server compiles it
  once into its cache directory. `--shell-command-spawn NAME`
  and `--shell-command-tunnels NAME` grant spawning and tunnels. See
  [Commands from a registry](./grpc-server.md#commands-from-a-registry).
- **JavaScript (`@eryx-org/eryx`):** a shell sandbox's `commands` takes each
  command as the URL of its `jco transpile --instantiation async` output;
  commands there run in batch rather than as a live pipeline (see the
  package's README).
- **Hosted SDKs** for eryx.run don't take your own command files.

### Precompiling

Compiling a command takes time at every start. To pay it once:

- `Command::load_cached(path, Some("sha256:<hex>"))` checks the digest and
  compiles the command once into eryx's cache directory (`commands/` under
  `~/.cache/eryx` on Linux), so later loads of the same file skip compiling.
  `Command::from_file_expecting(path, "sha256:<hex>")` checks without
  caching.
- `eryx-precompile compile count.wasm --output count.cwasm` writes a
  precompiled command, which `Command::from_precompiled_file` (`unsafe`:
  only load files you trust) and pyeryx's `Command.from_precompiled_file`
  load. It only loads in the eryx version that built it, so recompile after
  every eryx upgrade.

## Commands that run commands

A command registered with `allow_spawn` can run the sandbox's other
commands through `eryx:process`, as git runs `git` and its helpers: `run`
runs one to completion with its output captured (up to 16 MiB each of
stdout and stderr), and `pipe` starts one with live pipes to its stdin and
stdout. Without `allow_spawn`, calls fail with `spawn-failed`.

Vendor the WIT from the eryx release you target,
[`crates/eryx-runtime/wit/deps/process.wit`](https://github.com/eryx-org/eryx/blob/eryx-v0.16.0/crates/eryx-runtime/wit/deps/process.wit)
at `eryx-v0.16.0`, as `wit/deps/process.wit`, next to a world importing
what you use:

```wit
// wit/world.wit
package example:each;

world each {
    import eryx:process/run@0.1.0;
}
```

Then, with `wit-bindgen = "0.63"`, a command that runs `CMD ARG... LINE`
for each line of its stdin:

```rust,ignore
use std::io::{BufRead, Write};

wit_bindgen::generate!({ world: "each", path: "wit", generate_all });
use crate::eryx::process::run::run;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("usage: each CMD [ARG]...");
        std::process::exit(2);
    };
    let args: Vec<String> = args.collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = std::env::var("PWD").unwrap_or_else(|_| "/".into());
    let mut status = 0;
    for line in std::io::stdin().lock().lines() {
        let mut argv = args.clone();
        argv.push(line.expect("reading stdin"));
        match run(&cmd, &argv, &env, &cwd, &[]) {
            Ok(out) => {
                std::io::stdout().write_all(&out.stdout).unwrap();
                std::io::stderr().write_all(&out.stderr).unwrap();
                if out.status != 0 {
                    status = out.status;
                }
            }
            Err(e) => {
                eprintln!("each: {cmd}: {e:?}");
                std::process::exit(126);
            }
        }
    }
    std::process::exit(status);
}
```

```rust,ignore
let sandbox = Sandbox::embedded()
    .shell()
    .with_command("each", Command::from_file("each.wasm")?.allow_spawn())
    .with_command("count", Command::from_file("count.wasm")?)
    .build()?;
sandbox.execute("printf 'a b\\n' > /tmp/a.txt; ls /tmp/*.txt | each count").await?;
```

- `cmd` is a registered name (or `sh`/`bash`), not a path. Pass `PWD` as
  `cwd` so the command it runs starts where it does.
- `sh -c '...'` and `bash` work from a spawning command: they run the
  embedded shell, with the commands the execution allows.
- Children may spawn four levels deep (the shell's own commands are the
  first), share the execution's fuel, timeout, memory pool and child limits,
  and a spawned command's network is only what both it and its caller are
  allowed (see [External Commands](./sandboxes.md#external-commands)).

## Distributing

- **A file and its digest:** publish the `.wasm` with its sha256, and have
  hosts pin it: `Command::load_cached(path, Some("sha256:<hex>"))` or
  `from_file_expecting` in Rust, `@sha256:<hex>` on eryx-server's
  `--shell-command`. Publish the `.wasm`, not a `.cwasm`: a precompiled file
  is native code tied to one eryx version and CPU, and can't be validated.
- **An OCI registry:** [wkg](https://github.com/bytecodealliance/wasm-pkg-tools)
  pushes a component as a single-layer Wasm artifact, and pulls it back
  to a file:

  ```bash
  wkg oci push ghcr.io/you/commands/count:0.1.0-r.1 out/count/component.wasm
  wkg oci pull ghcr.io/you/commands/count:0.1.0-r.1 -o count.wasm
  ```

  The pin is the layer digest, which is the sha256 of the `.wasm` itself. It
  isn't the manifest digest that `wkg oci push` prints. eryx has no registry
  client: fetch commands before the host starts (an init container for
  eryx-server), then load them pinned. With oras, fetch the layer with
  `oras blob fetch`; `oras pull` skips it, since wkg's layer has no file
  name. See [Commands from a Registry](./precompile.md#commands-from-a-registry).
- **Versions:** `<upstream>-r.<n>` (for example `15.1.0-r.2`) tells a
  rebuild of the same upstream release apart; the eryx-build README says
  when to start a new revision.
- **Licensing:** a command keeps its own licence. A sandbox loads commands
  at run time, so a GPL command can stay a separate artifact rather than
  part of an MIT or Apache-licensed host binary; eryx publishes its GPL
  commands (git) apart from the others, with their source alongside. Ship
  each command's licence and third-party notices with it.
