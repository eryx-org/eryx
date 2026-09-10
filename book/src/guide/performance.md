# Performance Tuning

Most of the cost of a short `Sandbox::execute()` is not running Python. It is
creating the WebAssembly instance the code runs on and tearing it down again:
mapping the pre-initialized heap image, building a function reference for
every entry in the runtime's ~6k-entry function table, and, afterwards,
resetting the memory slot. Running `pass` on a fresh instance takes about
1.3 ms on a fast workstation; the Python part of that is roughly 0.2 ms.

Eryx hides that cost in two ways, both on by default. This page explains what
they do and the environment variables that tune them. All of these are read
once, when the process-wide wasmtime engine is created, so set them in the
environment of the process that embeds eryx (for `pyeryx`, the Python process).

## Warm instances

After every stateless execution, a background task instantiates a replacement
store for the same runtime and parks it in a process-wide pool. The next
`execute()` takes it instead of instantiating, and the used store is dropped on
the background task too. Every execution still runs on an instance that has
never run user code, so isolation is unchanged; only the timing moves.

The pool is keyed by runtime component rather than by `Sandbox`, so the common
pattern of building a short-lived sandbox per request (for example
`SandboxFactory.create_sandbox()` from Python) benefits from it: the first
request in a process pays for instantiation, later ones do not.

| Variable | Default | Effect |
|----------|---------|--------|
| `ERYX_WARM_INSTANCES` | `1` | Instances to keep ready per runtime. `0` disables the pool. |

One ready instance is enough for a caller that executes serially. If several
tasks execute concurrently, raise it towards that concurrency; each ready
instance costs about the resident size of the pre-initialized heap (tens of
megabytes, mostly shared copy-on-write pages) plus wasmtime's per-instance
metadata.

The pool is only used on a multi-threaded Tokio runtime, where the background
work actually runs in parallel. On a current-thread runtime it would only add to
the next request's latency, so it stays off. `PythonExecutor::warm_instances_ready()`
reports how many instances are waiting for that executor's runtime.

## Instance allocation

Wasmtime can allocate each instance's linear memory, tables, and async stacks
on demand (a fresh `mmap` per instance) or from a pool of pre-reserved slots.
Eryx uses the pooling allocator: a slot that has just been released is reused
by the next instance, so its pages are still mapped, and on Linux only the
pages the previous instance dirtied are reset.

| Variable | Default | Effect |
|----------|---------|--------|
| `ERYX_ALLOCATOR` | `pooling` | `pooling` or `on-demand`. |
| `ERYX_POOL_INSTANCES` | `1000` | Maximum instances alive at once. Every running `execute()` and every live session holds one; instantiation fails once the pool is full. |
| `ERYX_POOL_KEEP_RESIDENT_MB` | `64` | How much of each linear memory to keep mapped between uses. |
| `ERYX_POOL_PAGEMAP_SCAN` | `1` | `1`: reset only dirty pages (Linux 6.7+, via `PAGEMAP_SCAN`). `0`: `memcpy` the whole keep-resident budget instead, which trades a larger copy for fewer page faults on the next execution. |

The pool reserves virtual address space for every slot up front — several
terabytes with the defaults, which is normal for wasmtime deployments but can
be refused by a host with a low `ulimit -v`. If the pool cannot be created, eryx
logs a warning and falls back to on-demand allocation; execution behaves
identically either way.

If a process holds more than `ERYX_POOL_INSTANCES` sessions open at once,
raise the limit or switch to `on-demand`. Note that the choice of allocator does
not affect precompiled `.cwasm` artifacts; only compilation settings do.

## Host heap

Wasmtime keeps each instance's metadata (the `VMContext`, about 450 KB for this
runtime's ~10k function references) on the ordinary host heap and frees it when
the instance is torn down. With glibc's default `malloc` settings that block is
usually the top of the heap, so freeing it hands the pages back to the kernel
and the next instantiation grows the heap and faults them in again: three `brk`
calls and roughly 100 page faults per execution, or about a quarter of a cold
`pass` once the allocator above is in place.

On Linux with glibc, eryx therefore sets two `malloc` tunables once, when the
engine is created: trimming is disabled (`M_TRIM_THRESHOLD = -1`) and the heap
grows in 64 MiB steps (`M_TOP_PAD`). Both are process-wide; the cost is that
freed heap stays mapped in the process instead of being returned to the OS.

| Variable | Default | Effect |
|----------|---------|--------|
| `ERYX_GLIBC_MALLOC_TUNING` | `1` | `0` leaves `malloc` untouched. |

Processes that use another allocator (jemalloc, mimalloc, or musl's) are not
affected either way; check whether that allocator returns freed memory eagerly
if you see `brk`/`munmap` churn in `strace` around executions.

## Measuring

`crates/eryx/examples/profile_stateless.rs` times the stateless path and is a
convenient target for `perf` or `samply`:

```bash
cargo build --example profile_stateless --features embedded --release
# 2000 executions of `pass`, with a 2 ms gap between them so the background
# replenishment gets the same chance it has between real requests
./target/release/examples/profile_stateless 2000 pass 2000
ERYX_WARM_INSTANCES=0 ./target/release/examples/profile_stateless 2000 pass 2000
```

The criterion benchmarks (`cargo bench --package eryx --features embedded`)
cover the same paths with callbacks registered.
