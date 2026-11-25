# eryx-linux (spike)

A real Linux guest for eryx. It boots [unkernel] (Linux 7.x compiled to
wasm, with kwemu, an aarch64 CPU emulator, linked in for user programs)
under wasmtime, so unmodified Alpine packages run in the sandbox:
`apk add git jq ripgrep sqlite python3 curl` works, and so do the tools.

This is a spike, not part of eryx's API, and it isn't built by the
workspace or CI. It parks two dependencies that aren't ready for
production: unkernel itself (days old, one author, a host interface that
changes daily) and wasmtime's experimental stack switching.

## How it works

- **No JSPI.** unkernel's JS host switches kernel tasks with JSPI.
  `src/switcher.wat` runs each kernel task as a wasm continuation instead
  (`cont.new`/`resume`/`suspend`). That needs wasmtime's stack switching:
  x86_64 Linux or macOS, cranelift, inlining off.
- **The host** (`src/main.rs`) is a port of the host half of unkernel's
  MIT-licensed JS SDK: WASI console and initrd, the block device (the
  erofs distro image), and kwemu's JIT hook (one wasm module per hot block).
- **Network** (`src/net.rs`): a slirp-style user-mode stack. ARP, DHCP and
  DNS are answered locally; guest TCP is terminated into host sockets. It
  has **no policy yet**.
- **Exec** (`src/vsock.rs`): vsock plus unkernel's `kwtty` protocol.
  `--exec CMD` runs `sh -c CMD` in the guest and returns stdout, stderr and
  the exit status separately.
- **Snapshots** (`src/snapshot.rs`):
  - `--snapshot-out` parks the kernel at a consistent point and saves the
    memory in use.
  - `--snapshot-in` resumes a fresh instance through `kw_resume`.
  - `--cow` bakes the snapshot into a precompiled memory-image module that
    wasmtime maps copy-on-write. This needs `memory_guaranteed_dense_image_size`
    raised, writes the image sparse, and uses a variant of the kernel module
    with no start function and passive data, so instantiating it leaves
    memory alone.

## Running it

To play with it, fetch the kernel and run it. You get an Alpine shell with
the network on, so `apk add` works:

```sh
./fetch-unkernel.sh
cargo run --release --bin eryx-linux     # Ctrl-] quits; Ctrl-C goes to the guest
```

The terminal is in raw mode while the guest owns it: arrows, tab
completion, Ctrl-C and editors all work, and the guest console takes your
terminal's size. With `vendor/` present, `--kernel` and `--distro` default
to the fetched files.

Scripted use:

```sh
K=vendor/unkernel.wasm D=vendor/alpine.erofs
./target/release/eryx-linux --kernel $K --distro $D --exec 'uname -a'
./target/release/eryx-linux --kernel $K --distro $D --exec true --snapshot-out alpine.snap
./target/release/eryx-linux --kernel $K --distro $D --snapshot-in alpine.snap --cow \
    --exec 'echo hi' --repeat 50 --keep
```

Other flags:

- `--no-net`, `--no-jit` (the JIT is often a net loss, see below)
- `--mem MiB` (default 512)
- `--input SCRIPT`: console input, with `{N}` waiting N seconds
- `--timeout S`

The comparison benchmark, `src/bin/eryx_shell.rs`, embeds eryx's shell
runtime, so it sits behind a feature. It also needs the paths of eryx's
precompiled runtimes:

```sh
ERYX_RUNTIME_CWASM=... ERYX_RUNTIME_CWASM_SHELL=... ERYX_COREUTILS_CWASM=... \
    cargo run --release --features compare --bin eryx_shell
```

## What we measured (2026-11-24, x86_64)

| | eryx shell (brush + coreutils) | eryx-linux |
| --- | --- | --- |
| New sandbox to first command | 44–84 ms to build | ~5 ms from a CoW snapshot (112–200 ms cold boot) |
| Memory per live sandbox | | ~11 MiB with `--cow` (29 MiB copied) |
| 20k-iteration shell loop | 245 ms | 0.7 s JIT / 1.6 s interpreter |
| `seq 100000 \| sort -r` | 31 ms | 2.4 s / 4.0 s |
| `gh --version` | | 0.76 s (22 ms native) |
| `go build` hello, incremental | | 4.2 s (0.1 s native) |

Notes on these numbers:

- Startup and anything kernel-bound (files, fork, git, small tools) are
  close to eryx. Compute runs 35–50× slower than native: kwemu emulates
  aarch64 user code.
- The JIT pays cranelift about 5 ms per block, synchronously. It made big
  programs 2–4× slower: on a cold `go build` it spent 670 s compiling and
  hit wasmtime's limit of 10,000 instances per store.
- container2wasm (whole-machine emulation) on the same workloads: 436 ms
  per start, and 2–4× slower than eryx-linux's interpreter.

## Known gaps

**Safety:**
- Device imports index guest memory with guest-supplied pointers. A bad
  one panics the host. Every import needs checked access.
- Every sandbox resumed from one snapshot starts with the same kernel
  randomness; it needs reseeding after resume.
- No CPU or time limits, and no network policy. Port 443 could be
  intercepted host-side with an eryx CA in the guest image, as eryx does
  for TLS today.

**Spike limitations:**
- JIT instances and 2 MiB continuation stacks are never freed while a
  sandbox lives.
- One CPU per sandbox.
- IPv4 TCP only.
- aarch64 guest binaries only.

## Licences

- This crate's code is MIT OR Apache-2.0, like eryx. Parts of the host are
  ported from unkernel's MIT-licensed JS SDK; its notice is in
  `LICENSE-unkernel-MIT`.
- The kernel (`unkernel.wasm`) is GPL-2.0-only. So is the resume variant
  `--cow` derives from it, and so are snapshots, which contain kernel
  memory. None of them are committed: `fetch-unkernel.sh` downloads the
  kernel from unkernel's release, and the rest is generated locally.
  Distributing any of them brings GPL obligations, which is the #837
  licensing question.

[unkernel]: https://github.com/pithings/unkernel
