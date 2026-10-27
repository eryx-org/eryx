# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## `eryx-precompile` - [0.12.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.11.0...eryx-precompile-v0.12.0) - 2026-10-27

### Security
- `setup` checks the download against the release's `runtime.wasm.sha256` asset (or `--sha256`), caps its size, and writes the cache atomically to a private per-user directory, reusing an entry only if it matches its checksum ([#651](https://github.com/eryx-org/eryx/pull/651), [#655](https://github.com/eryx-org/eryx/pull/655)). See the `eryx` 0.12.0 notes for details.

### Fixed
- *(eryx)* [**breaking**] per-user cache directories and unsafe cache configuration ([#655](https://github.com/eryx-org/eryx/pull/655))
- *(precompile)* [**breaking**] verify and bound the setup download, write the cache atomically ([#651](https://github.com/eryx-org/eryx/pull/651))
- precompile CLI flag handling and #[callback] macro edge cases ([#614](https://github.com/eryx-org/eryx/pull/614))

## `eryx` - [0.12.0](https://github.com/eryx-org/eryx/compare/eryx-v0.11.0...eryx-v0.12.0) - 2026-10-27

This is a hardening release: it closes several ways sandboxed code could get past secret, network, filesystem and resource policy, and almost every package has breaking changes. Read **Security** and **Upgrade notes** before upgrading.

### Security
- **Upgrade recommended for anyone using secrets.** A script could get a secret to a host it wasn't meant for, or read the real value, in several ways ([#656](https://github.com/eryx-org/eryx/pull/656), [#637](https://github.com/eryx-org/eryx/pull/637), [#661](https://github.com/eryx-org/eryx/pull/661)):
  - A secret with no `allowed_hosts` of its own, under a `NetConfig` with no `allowed_hosts` (for example `allow_all_hosts`, which was the `pyeryx` default), was substituted for every host. It is now never substituted, and `SandboxBuilder::build` fails when networking is on. `eryx-server` rejects such requests with `INVALID_ARGUMENT`.
  - Substitution checked the host passed to `connect`, but the TLS server name and the `Host` header were up to the script. Behind a shared front end (a CDN or cloud load balancer) that let a script connect to an allowed host and have the request routed to a different site. Both must now name the connected host. One side effect: HTTPS through a `CONNECT` proxy no longer works.
  - An allowed host that echoes request headers back (a debug endpoint, some error pages) handed the real value to the script. Data read from a connection now has every real secret value replaced with its placeholder, including values split across reads.
  - Secrets were substituted on plain `http://` connections, so the real value crossed the network in cleartext. They are now substituted only after a TLS upgrade. `NetConfig::allow_plaintext_secrets(true)` restores the old behaviour (Rust only).
  - Placeholders are now also scrubbed from `InProcessSession` output, uncaught-exception tracebacks and the server's trace events ([#660](https://github.com/eryx-org/eryx/pull/660), [#657](https://github.com/eryx-org/eryx/pull/657)).

  Affects the `eryx` crate, the `pyeryx` wheels and the `sd2k/eryx-server` image.
- **Upgrade recommended on shared machines.** The embedded runtime and the native-extension component cache were extracted to fixed names in the shared temp directory (`$TMPDIR/eryx-embedded`, `$TMPDIR/eryx-cache`), and a file already there was reused because it existed. wasmtime loads these files as native code without validating them, so another local user who created that directory first could run code in any process using eryx. The cache now lives in a per-user directory (`$XDG_CACHE_HOME/eryx`, else `~/.cache/eryx` or the platform equivalent, else `$TMPDIR/eryx-<uid>`), which must be owned by you and not group- or world-writable, and an extracted runtime is compared with the embedded copy before reuse ([#655](https://github.com/eryx-org/eryx/pull/655)). `with_cache` / `with_cache_dir` are now `unsafe fn`, since the caller has to vouch for what is in the cache. `pyeryx`'s `SandboxFactory.load` refuses files other users can write. The `embedded` build only embeds a runtime cached by `eryx-precompile setup` if it matches its checksum and has safe permissions ([#662](https://github.com/eryx-org/eryx/pull/662)), and `setup` checks the download against the new `runtime.wasm.sha256` release asset, caps its size and writes the cache atomically ([#651](https://github.com/eryx-org/eryx/pull/651)). Affects the `eryx` crate with the `embedded` feature, the `pyeryx` wheels and `eryx-precompile`. Single-user machines and containers aren't exposed in practice.
- **`pyeryx` and its CLI allowed more network access than documented** ([#654](https://github.com/eryx-org/eryx/pull/654)):
  - `NetConfig()` allowed every public host by default. It is now an allowlist like the Rust `NetConfig::default()`: only `allowed_hosts` can be reached, and `allow_host()` turns `allow_all_hosts` off.
  - `eryx --net`, `eryx --allow-host` and `eryx serve` used `NetConfig.permissive()`, which has no blocked networks. Sandboxed code run through the CLI could reach localhost, private networks and the cloud metadata endpoint, and `--allow-host` didn't restrict anything. Both now keep the default blocks, and `--allow-host` is an allowlist.
  - `--mcp` also loaded MCP server configs from the current directory, so running `eryx --mcp` inside an untrusted checkout started whatever servers it listed. Project configs now need `--mcp-project`, and never replace a user-wide server with the same name.
  - `Session()` and `SandboxFactory.create_session()` ran with no resource limits. They now default to `ResourceLimits()` (30 s, 128 MB).
  - An exception raised by a host callback reached sandboxed code with its full host traceback. The script now sees `Type: message` only, and the traceback is logged on the `eryx` logger.
- **`@bsull/eryx` network policy could be bypassed** ([#650](https://github.com/eryx-org/eryx/pull/650), [#663](https://github.com/eryx-org/eryx/pull/663)):
  - The browser (fetch) transport built the request URL from what the script wrote, so an absolute-form request target could send the request to a host the policy never checked. Redirects were also followed to unchecked hosts. URLs are now built from the connection's host, and the transport no longer follows redirects, so a client that follows one has to connect again and goes through the policy.
  - Numeric host spellings such as `2130706433` and `0x7f.1` got past patterns like `127.*`. Hosts are now matched in WHATWG URL form as well.
  - `0.0.0.0` and `[::]` are now blocked by default, and `DEFAULT_BLOCKED_NETWORKS` matches the Rust list, including IPv4-compatible and Teredo addresses.
  - There were no connection limits or timeouts. `maxConnections` (10), `connectTimeoutMs` and `ioTimeoutMs` now match the Rust defaults, and connections are closed after each call.
- **`@bsull/eryx` sandboxes weren't isolated from each other.** Every `Sandbox` in a process drove one shared WebAssembly instance, so one sandbox could read and change another's Python state, files and handlers, and use its open connections. Each `Sandbox` now has its own instance, filesystem, handlers, connection table and network policy ([#652](https://github.com/eryx-org/eryx/pull/652), [#663](https://github.com/eryx-org/eryx/pull/663)). Nothing could interrupt a running call either; `new Sandbox({ timeoutMs })` now runs the sandbox in a Worker that is terminated on timeout. Timeouts are opt-in in JS, and there is still no memory limit.
- **Virtual filesystem and volume escapes** ([#658](https://github.com/eryx-org/eryx/pull/658)):
  - `..` and absolute paths could climb out of a VFS preopen into the rest of the VFS storage. They are now refused, as they already were for host directories.
  - `open_at` didn't check the preopen's `file_perms`, so files in a read-only VFS preopen could be opened for writing. It now returns `EACCES`.
  - Single-file volume mounts could be replaced by a directory, renamed, or followed as a symlink. Only the mapped file is exposed now, and its host path is resolved once at mount time.
  - Guests could create symlinks in writable host-directory volumes, which host programs reading that directory later might follow. Symlink creation on host directories is now refused.
- **Resource limits that weren't enforced, or didn't exist.** These let a script use up host memory, CPU, file descriptors or a worker:
  - `eryx-server` parsed the request's `max_memory_bytes` but didn't apply it, and a client disconnect didn't stop CPU-bound code, which kept a worker and a pooled sandbox busy until the timeout. Both are fixed. Operators can now cap every request limit with `--max-*` ceilings (requests above a ceiling are clamped), and output is capped at 16 MiB per execution ([#657](https://github.com/eryx-org/eryx/pull/657), [#665](https://github.com/eryx-org/eryx/pull/665)).
  - `snapshot_state`, `restore_state` and `clear_state` run guest code (`__reduce__`, `__del__`) with no time or fuel bound, so they could hang forever. They now always have a time bound and can be cancelled ([#660](https://github.com/eryx-org/eryx/pull/660), [#665](https://github.com/eryx-org/eryx/pull/665)).
  - Trace events and streamed output were queued without bound. Each execution now queues at most 16 MiB of each ([#660](https://github.com/eryx-org/eryx/pull/660)).
  - Writes to fd 1 and 2 (`os.write(1, ...)`, `sys.__stdout__`) went straight to the host process's stdout and stderr, unscrubbed. They are now captured into the execution's output ([#660](https://github.com/eryx-org/eryx/pull/660), [#665](https://github.com/eryx-org/eryx/pull/665)).
  - Open host files and directories are capped at 256 per sandbox and at a process-wide budget (half the soft `RLIMIT_NOFILE` by default). In-memory VFS entries are capped in count, name and path length and charged a per-entry overhead, so many empty files can't get around `max_bytes`. Volume file size can be capped with `VolumeMount::with_max_file_size` ([#658](https://github.com/eryx-org/eryx/pull/658), [#664](https://github.com/eryx-org/eryx/pull/664)).
  - HTTP request header buffering is capped at 64 KiB, and the header scan is no longer quadratic ([#656](https://github.com/eryx-org/eryx/pull/656)).
  - Package extraction rejects tar symlink and special entries and caps archives at 100k entries and 1 GiB extracted ([#620](https://github.com/eryx-org/eryx/pull/620)).
- **Callback replay journals were bound to too little.** The journal MAC didn't cover the starting `state_snapshot`, the declared callback names or an expiry, so a signed journal could be replayed against different state, with different callbacks, indefinitely. It now covers all three; journals expire after 7 days by default (`--journal-ttl-secs`) ([#657](https://github.com/eryx-org/eryx/pull/657)).
- The `sd2k/eryx-server` image now runs as a non-root user (UID/GID 10001) ([#635](https://github.com/eryx-org/eryx/pull/635)).
- More default blocked networks in Rust: `::/96` (replacing `::/128` and `::1/128`), `fec0::/10`, `2001::/32` and `2001:db8::/32`. IPv4-compatible and Teredo addresses are checked by the IPv4 address they embed, like NAT64 ([#656](https://github.com/eryx-org/eryx/pull/656)).

### Highlights
- **`asyncio.run()` works in the sandbox** ([#639](https://github.com/eryx-org/eryx/pull/639)). Top-level `asyncio.run(main())` is run on the sandbox's event loop, so sync-style scripts work as written. Traceback line numbers in user code are no longer shifted by the preamble.
- **`pyeryx` async and cancellation** ([#641](https://github.com/eryx-org/eryx/pull/641)). `execute_async()` on `Sandbox`, `Session` and `PooledSandbox`, a `CancelToken` that can stop a run from any thread, and Ctrl-C now interrupts a blocking `execute()`. Rust sessions gain `SessionExecuteBuilder::with_cancellation`.
- **Per-instance JS sandboxes with timeouts** ([#652](https://github.com/eryx-org/eryx/pull/652), [#663](https://github.com/eryx-org/eryx/pull/663)). See Security above.
- **`eryx-server` operations** ([#635](https://github.com/eryx-org/eryx/pull/635), [#657](https://github.com/eryx-org/eryx/pull/657)). A `grpc.health.v1.Health` service, HTTP/2 keepalive, a per-connection stream limit, idle sandbox eviction, operator ceilings on request limits and `ExecuteResult.output_truncated`.
- **API stability pass before 1.0** ([#667](https://github.com/eryx-org/eryx/pull/667)). `NetConfig` and `SecretConfig` fields are private, `eryx::CancellationToken` is a newtype, output types are `#[non_exhaustive]` and `eryx::vfs` is documented as outside semver, so after 1.0 new fields and dependency upgrades won't be breaking.
- **Session state reset reporting** ([#637](https://github.com/eryx-org/eryx/pull/637)). `ExecuteResult::state_was_reset` is `true` on the first run after a session lost its state.
- **Faster `clear_state()` then execute** ([#674](https://github.com/eryx-org/eryx/pull/674)). Clearing or restoring session state no longer recompiles the callback setup script, it reuses the compiled copy for the current callback declarations. In a focused benchmark, clearing state and then executing went from 7.96 ms to 1.27 ms.

### Upgrade notes

**Rust (`eryx` crate)**
- A secret needs `allowed_hosts`, of its own or on the `NetConfig` (without `allow_all_hosts`), or `build()` fails. Secrets are substituted only over TLS; use `NetConfig::allow_plaintext_secrets(true)` if you really need `http://` ([#656](https://github.com/eryx-org/eryx/pull/656), [#637](https://github.com/eryx-org/eryx/pull/637)).
- `NetConfig` and `SecretConfig` can't be built with struct literals. Use the builders, `SecretConfig::new(value, hosts)` and the read accessors. `generate_placeholder` is removed. A zero connect or I/O timeout now fails `build()` ([#667](https://github.com/eryx-org/eryx/pull/667)).
- `with_cache` and `with_cache_dir` are `unsafe fn`. The default cache needs no `unsafe`. If `~/.cache/eryx` already exists and is group- or world-writable, eryx falls back to `$TMPDIR/eryx-<uid>`; run `chmod 700 ~/.cache/eryx` to use it again. Old `$TMPDIR/eryx-embedded` and `$TMPDIR/eryx-cache` directories are no longer used or cleaned up ([#655](https://github.com/eryx-org/eryx/pull/655)).
- `SandboxBuilder` now defaults to `with_trace_collection(false)` ([#660](https://github.com/eryx-org/eryx/pull/660)).
- Duplicate callback names fail `build()`. `Sandbox::set_callbacks` and `PooledSandbox::with_callbacks` return `Result` ([#637](https://github.com/eryx-org/eryx/pull/637)).
- Session state ops can return `Error::Timeout`, `Error::FuelExhausted` or `Error::Cancelled`, and default to a 30 s bound without an execution timeout. `SessionExecutor::state_op_timeout()` returns `Duration` ([#660](https://github.com/eryx-org/eryx/pull/660), [#665](https://github.com/eryx-org/eryx/pull/665)).
- `eryx::CancellationToken` is no longer the tokio-util type. `ReplayOutcome`, `TraceEvent`, `SuspendedCallback`, `CallbackRequest` and `OutputRequest` are `#[non_exhaustive]`; use `CallbackRequest::new` / `OutputRequest::new` in tests ([#667](https://github.com/eryx-org/eryx/pull/667)).
- `SandboxPool::new` rejects builder settings it can't keep, `try_acquire` no longer creates sandboxes, and `PoolError::Exhausted` / `PoolError::Reset` are removed. Dropping an unfinished `ExecutionHandle` cancels it ([#620](https://github.com/eryx-org/eryx/pull/620)).
- `VolumeMount` has a new `max_file_size` field. `SnapshotSession` is removed ([#664](https://github.com/eryx-org/eryx/pull/664), [#615](https://github.com/eryx-org/eryx/pull/615)).

**Python (`pyeryx`)**
- `NetConfig()` allows no hosts unless you set `allowed_hosts` or pass `allow_all_hosts=True` ([#654](https://github.com/eryx-org/eryx/pull/654)).
- `Session()` and `create_session()` apply `ResourceLimits()`; pass `resource_limits=ResourceLimits.unlimited()` for the old behaviour ([#654](https://github.com/eryx-org/eryx/pull/654)).
- `--net` and `--allow-host` no longer reach localhost or private networks, and `--mcp` needs `--mcp-project` for project configs ([#654](https://github.com/eryx-org/eryx/pull/654)).
- `PoolExhaustedError` is removed. `eryx.TimeoutError` is now an `EryxError`, and uncaught script exceptions raise the new `PythonExceptionError` (still an `ExecutionError`) ([#638](https://github.com/eryx-org/eryx/pull/638), [#617](https://github.com/eryx-org/eryx/pull/617)).

**JavaScript (`@bsull/eryx`)**
- Sandboxes no longer share interpreter state with each other or with the module-level `execute()`. The module-level `setResultVariable()` and `setNetConfig()` only affect the default sandbox and sandboxes created afterwards; use `new Sandbox({ netConfig })`, `sandbox.setNetConfig()` and `sandbox.setResultVariable()` ([#652](https://github.com/eryx-org/eryx/pull/652), [#663](https://github.com/eryx-org/eryx/pull/663)).
- The fetch transport no longer follows redirects. At most 10 connections are open by default, and connections are closed after each call ([#650](https://github.com/eryx-org/eryx/pull/650)).
- `./low-level` now exports jco's `instantiate()` ([#652](https://github.com/eryx-org/eryx/pull/652)).

**gRPC server (`sd2k/eryx-server`)**
- The image runs as UID 10001. Files it reads (`--runtime-cwasm`, `--stdlib`, TLS keys) must be readable by that UID, and with a read-only root filesystem `/tmp` must be writable ([#635](https://github.com/eryx-org/eryx/pull/635)).
- `max_memory_bytes` is now enforced; scripts that used more fail with `MemoryError`. The server's `--max-memory-bytes` ceiling defaults to 512 MiB ([#657](https://github.com/eryx-org/eryx/pull/657)).
- Callback journals signed by an older server no longer verify (`FAILED_PRECONDITION`), so re-run any suspended executions pending at deploy time. Resumes must use the same callback names and `state_snapshot`, within the TTL, and echo the journal back whole ([#657](https://github.com/eryx-org/eryx/pull/657)).
- Secrets with no allowed hosts are rejected with `INVALID_ARGUMENT`. Requests with unsafe or duplicate file names, invalid or duplicate callback names, or an invalid `result_variable` are rejected too ([#661](https://github.com/eryx-org/eryx/pull/661), [#616](https://github.com/eryx-org/eryx/pull/616)).
- Messages up to 32 MiB are accepted and sent; clients receiving large snapshots need to raise their receive limit (grpc-go `MaxCallRecvMsgSize`). At most 200 concurrent streams per connection, and sandboxes beyond `--pool-min-idle` are evicted after 5 minutes idle ([#616](https://github.com/eryx-org/eryx/pull/616), [#635](https://github.com/eryx-org/eryx/pull/635)).
- `--max-output-bytes` above 16 MiB is refused at startup ([#665](https://github.com/eryx-org/eryx/pull/665)).

**`eryx-precompile`**
- `setup --version <x>` needs `--sha256 <hex>` for releases without a `runtime.wasm.sha256` asset (0.11.0 and earlier). `--site-packages`, `--package` and `--import` require `--preinit` ([#651](https://github.com/eryx-org/eryx/pull/651), [#614](https://github.com/eryx-org/eryx/pull/614)).

### Added
- [**breaking**] API stability pass before 1.0 ([#667](https://github.com/eryx-org/eryx/pull/667))
- *(vfs)* [**breaking**] make volume file size and open host file limits configurable ([#664](https://github.com/eryx-org/eryx/pull/664))
- [**breaking**] bound and cancel session state ops, report all output truncation ([#665](https://github.com/eryx-org/eryx/pull/665))
- *(python)* execute_async() and CancelToken with Ctrl-C support ([#641](https://github.com/eryx-org/eryx/pull/641))
- *(guest)* support asyncio.run() and keep user-code line numbers after a preamble ([#639](https://github.com/eryx-org/eryx/pull/639))
- *(eryx)* [**breaking**] TLS-only secret substitution, unique callback names, session reset reporting ([#637](https://github.com/eryx-org/eryx/pull/637))
- *(vfs)* path-based scrub policies, stream and mkdir_sync lock fixes ([#636](https://github.com/eryx-org/eryx/pull/636))
- *(server)* [**breaking**] health service, keepalive, stream limits and non-root image ([#635](https://github.com/eryx-org/eryx/pull/635))

### Fixed
- *(eryx)* check the cached runtime before embedding it ([#662](https://github.com/eryx-org/eryx/pull/662))
- *(eryx)* [**breaking**] bound session state ops, capture fd output, limit trace and output streams ([#660](https://github.com/eryx-org/eryx/pull/660))
- *(eryx)* [**breaking**] per-user cache directories and unsafe cache configuration ([#655](https://github.com/eryx-org/eryx/pull/655))
- *(net)* [**breaking**] bind secret substitution to the connected host ([#656](https://github.com/eryx-org/eryx/pull/656))
- *(vfs)* [**breaking**] tighten path handling and limits in the virtual filesystem ([#658](https://github.com/eryx-org/eryx/pull/658))
- *(runtime)* provide writable tmp during pre-initialization ([#649](https://github.com/eryx-org/eryx/pull/649))
- *(guest)* make mounted site-packages importable on the pre-initialized runtime ([#631](https://github.com/eryx-org/eryx/pull/631))
- *(guest)* keep the interpreter alive on SystemExit and tidy the runtime ([#621](https://github.com/eryx-org/eryx/pull/621))
- *(build)* [**breaking**] detect stale late-linking artifacts, pin the guest toolchain, tidy preinit ([#618](https://github.com/eryx-org/eryx/pull/618))
- *(eryx)* [**breaking**] sandbox, pool, cache and package correctness fixes ([#620](https://github.com/eryx-org/eryx/pull/620))
- [**breaking**] networking and session correctness fixes ([#615](https://github.com/eryx-org/eryx/pull/615))
- *(vfs)* [**breaking**] VFS correctness fixes (renames, streams, host errors, set_times) ([#619](https://github.com/eryx-org/eryx/pull/619))

### Other
- *(runtime)* reuse compiled callback setup after clearing session state ([#674](https://github.com/eryx-org/eryx/pull/674))

## `eryx-vfs` - [0.12.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.11.0...eryx-vfs-v0.12.0) - 2026-10-27

### Security
- VFS paths can no longer climb out of their preopen, `open_at` enforces the preopen's `file_perms`, single-file mounts expose only the mapped file, and guests can't create symlinks in host directories. Open host files, in-memory entry counts and name and path lengths are now capped ([#658](https://github.com/eryx-org/eryx/pull/658), [#664](https://github.com/eryx-org/eryx/pull/664)). See the `eryx` 0.12.0 notes for details.

### Added
- [**breaking**] API stability pass before 1.0 ([#667](https://github.com/eryx-org/eryx/pull/667))
- *(vfs)* [**breaking**] make volume file size and open host file limits configurable ([#664](https://github.com/eryx-org/eryx/pull/664))
- *(vfs)* path-based scrub policies, stream and mkdir_sync lock fixes ([#636](https://github.com/eryx-org/eryx/pull/636))

### Fixed
- *(vfs)* [**breaking**] tighten path handling and limits in the virtual filesystem ([#658](https://github.com/eryx-org/eryx/pull/658))
- *(vfs)* [**breaking**] VFS correctness fixes (renames, streams, host errors, set_times) ([#619](https://github.com/eryx-org/eryx/pull/619))

## `eryx-runtime` - [0.12.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.11.0...eryx-runtime-v0.12.0) - 2026-10-27

### Added
- *(guest)* support asyncio.run() and keep user-code line numbers after a preamble ([#639](https://github.com/eryx-org/eryx/pull/639))

### Fixed
- *(runtime)* provide writable tmp during pre-initialization ([#649](https://github.com/eryx-org/eryx/pull/649))
- *(guest)* make mounted site-packages importable on the pre-initialized runtime ([#631](https://github.com/eryx-org/eryx/pull/631))
- *(guest)* keep the interpreter alive on SystemExit and tidy the runtime ([#621](https://github.com/eryx-org/eryx/pull/621))
- *(build)* [**breaking**] detect stale late-linking artifacts, pin the guest toolchain, tidy preinit ([#618](https://github.com/eryx-org/eryx/pull/618))

### Other
- *(runtime)* reuse compiled callback setup after clearing session state ([#674](https://github.com/eryx-org/eryx/pull/674))
- fail the build when the committed prebuilt/ is stale ([#633](https://github.com/eryx-org/eryx/pull/633))

## `eryx-macros` - [0.12.0](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.11.0...eryx-macros-v0.12.0) - 2026-10-27

### Fixed
- precompile CLI flag handling and #[callback] macro edge cases ([#614](https://github.com/eryx-org/eryx/pull/614))

## `eryx-precompile` - [0.11.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.10.1...eryx-precompile-v0.11.0) - 2026-10-23

### Other
- update Cargo.lock dependencies

## `eryx` - [0.11.0](https://github.com/eryx-org/eryx/compare/eryx-v0.10.1...eryx-v0.11.0) - 2026-10-23

### Security
- **Upgrade recommended for anyone using sandboxed networking with `allow_all_hosts`.** Before this release, network policy only matched the hostname a script asked for, before DNS. Any name that resolved to a loopback, private, link-local or other non-public address got through, including the cloud metadata endpoint `169.254.169.254`. Host patterns such as `127.*` could not catch it. `pyeryx` turns on `allow_all_hosts` when no allowlist is given, so most `pyeryx` users with networking enabled were exposed. eryx now resolves the name, checks every address against `blocked_networks` (which covers those ranges by default, including IPv4-mapped and NAT64 forms) and connects only to addresses that passed ([#606](https://github.com/eryx-org/eryx/pull/606)). This affects the `eryx` crate, the `pyeryx` wheels and the `sd2k/eryx-server` image. `@bsull/eryx` (npm) applies the same check when running on Node. Browsers can't see resolved addresses, so the check doesn't run there.
- **Breaking:** hosts that resolve to blocked networks are now refused even when allowlisted. To reach an internal service on purpose, add its hostname to `internal_hosts` (for example a cluster-internal name with no public DNS). You can also allow specific ranges with `allowed_networks`, or turn the check off by clearing `blocked_networks`. In gRPC an empty list means the defaults, so use `allowed_networks = ["0.0.0.0/0", "::/0"]` there instead.
- The 0.10.1 eMudhra fix was incomplete. `webpki-roots` 1.0.10 removed only emSign ECC Root CA - G3, and the C1 and C3 roots affected by the [eMudhra intermediate key compromise](https://blog.cloudflare.com/emudhra-certificate-compromise/) were still trusted. `webpki-roots` and `webpki-root-certs` are now at 1.0.11, which removes them, and the minimum `webpki-roots` version is raised to `1.0.11` ([#605](https://github.com/eryx-org/eryx/pull/605)). The affected packages are the same as for 0.10.1: the `eryx` crate, the `pyeryx` wheels and the `sd2k/eryx-server` image. `@bsull/eryx` isn't affected.

### Fixed
- *(net)* [**breaking**] check resolved addresses against blocked networks ([#606](https://github.com/eryx-org/eryx/pull/606))

### Other
- *(eryx)* pool instances and pre-instantiate stores for stateless execution ([#411](https://github.com/eryx-org/eryx/pull/411))

## `eryx-precompile` - [0.10.1](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.10.0...eryx-precompile-v0.10.1) - 2026-10-21

### Other
- update Cargo.lock dependencies

## `eryx` - [0.10.1](https://github.com/eryx-org/eryx/compare/eryx-v0.10.0...eryx-v0.10.1) - 2026-10-21

### Security
- **Upgrade recommended for anyone using sandboxed networking.** The TLS trust roots now exclude the eMudhra CAs affected by the [eMudhra intermediate key compromise](https://blog.cloudflare.com/emudhra-certificate-compromise/) ([#597](https://github.com/eryx-org/eryx/pull/597)). `webpki-roots` is the only trust store for HTTPS made from inside the sandbox, and its minimum version is raised to `1.0.10` so downstream crates can't resolve an older copy. `rustls-webpki` is also updated to 0.103.16. This affects the `eryx` crate, the `pyeryx` wheels and the `sd2k/eryx-server` image (whose rebuild also refreshes Debian's `ca-certificates`). `@bsull/eryx` (npm) doesn't bundle these trust roots, so it isn't affected.

### Other
- update Cargo.toml dependencies

## `eryx-vfs` - [0.10.1](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.10.0...eryx-vfs-v0.10.1) - 2026-10-21

### Other
- update Cargo.toml dependencies

## `eryx-runtime` - [0.10.1](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.10.0...eryx-runtime-v0.10.1) - 2026-10-21

### Other
- update Cargo.toml dependencies

## `eryx-precompile` - [0.10.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.9.1...eryx-precompile-v0.10.0) - 2026-10-19

### Other
- update Cargo.lock dependencies

## `eryx` - [0.10.0](https://github.com/eryx-org/eryx/compare/eryx-v0.9.1...eryx-v0.10.0) - 2026-10-19

### Added
- [**breaking**] pass the replay journal per execute call ([#530](https://github.com/eryx-org/eryx/pull/530))
- *(runtime)* asyncio timers in the sandbox event loop ([#528](https://github.com/eryx-org/eryx/pull/528))

### Fixed
- *(package)* reject wheel entries that escape the extraction dir ([#550](https://github.com/eryx-org/eryx/pull/550))
- *(replay)* key journals on the guest's raw argument JSON ([#529](https://github.com/eryx-org/eryx/pull/529))
- *(runtime)* cancel callback subtasks abandoned by an execution ([#527](https://github.com/eryx-org/eryx/pull/527))
- *(callbacks)* stop sessions hanging when a callback is abandoned ([#526](https://github.com/eryx-org/eryx/pull/526))
- *(runtime)* clear the event loop's exception after raising it ([#523](https://github.com/eryx-org/eryx/pull/523))

### Other
- cross-compile the Windows cwasm on Linux ([#579](https://github.com/eryx-org/eryx/pull/579))
- cut test wall time (nextest ~565s → ~185s, pytest xdist) ([#578](https://github.com/eryx-org/eryx/pull/578))
- *(deps)* update rust crate zip to v9 ([#533](https://github.com/eryx-org/eryx/pull/533))
- *(deps)* update dependency cargo:cargo-rail to v0.32.0 ([#543](https://github.com/eryx-org/eryx/pull/543))

## `eryx-runtime` - [0.10.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.9.1...eryx-runtime-v0.10.0) - 2026-10-19

### Added
- *(runtime)* asyncio timers in the sandbox event loop ([#528](https://github.com/eryx-org/eryx/pull/528))

### Fixed
- *(replay)* key journals on the guest's raw argument JSON ([#529](https://github.com/eryx-org/eryx/pull/529))
- *(runtime)* cancel callback subtasks abandoned by an execution ([#527](https://github.com/eryx-org/eryx/pull/527))
- *(runtime)* clear the event loop's exception after raising it ([#523](https://github.com/eryx-org/eryx/pull/523))

### Other
- *(deps)* update wasmtime & wasm-tools ([#554](https://github.com/eryx-org/eryx/pull/554))
- *(deps)* update rust crate zip to v9 ([#533](https://github.com/eryx-org/eryx/pull/533))

## `eryx-precompile` - [0.9.1](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.9.0...eryx-precompile-v0.9.1) - 2026-10-05

### Other
- update Cargo.lock dependencies

## `eryx` - [0.9.1](https://github.com/eryx-org/eryx/compare/eryx-v0.9.0...eryx-v0.9.1) - 2026-10-05

### Fixed
- *(wasm-runtime)* free async export call context after task_return ([#480](https://github.com/eryx-org/eryx/pull/480))

### Other
- *(result)* pin ensure_ascii escaping of lone surrogates ([#490](https://github.com/eryx-org/eryx/pull/490))

## `eryx-runtime` - [0.9.1](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.9.0...eryx-runtime-v0.9.1) - 2026-10-05

### Fixed
- *(wasm-runtime)* free async export call context after task_return ([#480](https://github.com/eryx-org/eryx/pull/480))

### Other
- *(deps)* update wasmtime & wasm-tools ([#497](https://github.com/eryx-org/eryx/pull/497))

## `eryx-precompile` - [0.9.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.8.0...eryx-precompile-v0.9.0) - 2026-09-22

### Other
- update Cargo.lock dependencies

## `eryx` - [0.9.0](https://github.com/eryx-org/eryx/compare/eryx-v0.8.0...eryx-v0.9.0) - 2026-09-22

### Fixed
- [**breaking**] stop stripping trailing newlines from stdout and stderr ([#468](https://github.com/eryx-org/eryx/pull/468))

## `eryx-vfs` - [0.9.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.8.0...eryx-vfs-v0.9.0) - 2026-09-22

### Other
- update Cargo.toml dependencies

## `eryx-runtime` - [0.9.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.8.0...eryx-runtime-v0.9.0) - 2026-09-22

### Fixed
- [**breaking**] stop stripping trailing newlines from stdout and stderr ([#468](https://github.com/eryx-org/eryx/pull/468))

## Bytes-native output (0.8.0 release notes)

**Release date:** 2026-09-19

This release changes stdout and stderr from strings to raw bytes across every
layer of the stack. Python code can emit arbitrary binary data and it will
arrive intact — no more silent UTF-8 replacement at the sandbox boundary.
Convenience helpers make the common "just give me text" path a one-line change.

This release also adds `SandboxPool` to the Python bindings, giving you bounded
concurrent execution with automatic lease lifecycle management.

### Highlights

- **stdout/stderr are now bytes** ([#448](https://github.com/eryx-org/eryx/pull/448)) —
  `ExecuteResult.stdout` and `.stderr` carry raw bytes everywhere: `Vec<u8>` in
  Rust, `bytes` in Python, `bytes` in the gRPC proto, and `Uint8Array` at the
  WIT boundary. The JS wrapper still decodes to `string` for you.
- **SandboxPool** ([#456](https://github.com/eryx-org/eryx/pull/456)) —
  factory-backed pool with bounded concurrency, automatic warm-up, per-request
  callbacks/limits, and context-manager lease lifecycle. Available in Rust and
  Python.
- **`NetRequest` is `#[non_exhaustive]`** ([#447](https://github.com/eryx-org/eryx/pull/447)) —
  future fields won't be breaking.
- **Explicit connection cleanup** ([#446](https://github.com/eryx-org/eryx/pull/446)) —
  reused sessions now release outbound connections between executions.

### Migration guide — Rust (`eryx` crate)

**`ExecuteResult.stdout` / `.stderr`: `String` → `Vec<u8>`**

```rust
// Before (0.7.x)
println!("{}", result.stdout);
if result.stderr.contains("Warning") { /* ... */ }

// After (0.8.0) — quick migration
println!("{}", result.stdout_text());
if result.stderr_text().contains("Warning") { /* ... */ }

// After (0.8.0) — strict UTF-8
let output = result.stdout_utf8()?;
```

**`OutputHandler` trait: `&str` → `&[u8]`**

```rust
// Before
async fn on_output(&self, chunk: &str) { /* ... */ }
async fn on_stderr(&self, chunk: &str) { /* ... */ }

// After
async fn on_output(&self, chunk: &[u8]) {
    let text = std::str::from_utf8(chunk).unwrap_or("�");
    // ...
}
async fn on_stderr(&self, chunk: &[u8]) { /* ... */ }
```

**`NetRequest` is `#[non_exhaustive]`** — add a `..` rest pattern if you
construct it directly:

```rust
// Before
let req = NetRequest { url, method, headers, body };

// After
let req = NetRequest { url, method, headers, body, ..Default::default() };
```

**`TraceEventKind` is `#[non_exhaustive]`** — add a wildcard arm to exhaustive
matches:

```rust
match event.kind {
    TraceEventKind::Line => { /* ... */ }
    TraceEventKind::Call { .. } => { /* ... */ }
    TraceEventKind::Return { .. } => { /* ... */ }
    _ => { /* future variants */ }
}
```

### Migration guide — Python (`pyeryx`)

**`ExecuteResult.stdout` / `.stderr`: `str` → `bytes`**

```python
# Before (0.7.x)
print(result.stdout)
if "error" in result.stderr:
    ...

# After (0.8.0) — quick migration (UTF-8 with replacement)
print(result.stdout_text)
if "error" in result.stderr_text:
    ...

# After (0.8.0) — raw bytes
sys.stdout.buffer.write(result.stdout)
```

**Streaming callbacks: `str` → `bytes`**

```python
# Before
def on_stdout(chunk: str) -> None:
    sys.stdout.write(chunk)

# After
def on_stdout(chunk: bytes) -> None:
    sys.stdout.buffer.write(chunk)
```

### Migration guide — JavaScript (`@bsull/eryx`)

**No breaking changes for most consumers.** The JS wrapper decodes
`Uint8Array` from the WASM boundary to `string` internally, so
`result.stdout` and `result.stderr` remain strings.

If you use the **streaming `outputHandler`**, the callback signature is
unchanged — it still receives `(stream: number, data: string)`. The decoding
now happens inside the shim via `TextDecoder` with proper multi-byte sequence
handling across chunks.

**If you access the raw WASM bindings directly** (bypassing the wrapper), the
WIT-level `report-output` import changed from `string` to `list<u8>`, and
`execute-output.stdout` / `.stderr` changed similarly.

### Migration guide — gRPC (`eryx.v1`)

**`ExecuteResult` and `OutputEvent` fields changed from `string` to `bytes`:**

```diff
 message OutputEvent {
   OutputStream stream = 1;
-  string data = 2;
+  bytes data = 2;
 }

 message ExecuteResult {
   bool success = 1;
-  string stdout = 2;
-  string stderr = 3;
+  bytes stdout = 2;
+  bytes stderr = 3;
   ...
 }
```

In most languages the generated types change from `string` to `bytes`/`[]byte`/
`ByteString`. For Go clients:

```go
// Before
fmt.Println(result.Stdout)

// After
fmt.Println(string(result.Stdout))
```

The wire format is backwards-compatible (protobuf `string` and `bytes` share
the same encoding), but **recompiling your client against the new proto is
required** to get the correct generated types.

---

## `eryx-precompile` - [0.8.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.7.2...eryx-precompile-v0.8.0) - 2026-09-19

### Added
- [**breaking**] change stdout/stderr from string to bytes across the full stack ([#448](https://github.com/eryx-org/eryx/pull/448))

## `eryx` - [0.8.0](https://github.com/eryx-org/eryx/compare/eryx-v0.7.2...eryx-v0.8.0) - 2026-09-19

### Added
- *(python)* expose factory-backed SandboxPool with safe lease lifecycle ([#456](https://github.com/eryx-org/eryx/pull/456))
- [**breaking**] change stdout/stderr from string to bytes across the full stack ([#448](https://github.com/eryx-org/eryx/pull/448))
- *(net)* add explicit connection cleanup for reused sessions ([#446](https://github.com/eryx-org/eryx/pull/446))

### Other
- *(net)* make NetRequest non-exhaustive ([#447](https://github.com/eryx-org/eryx/pull/447))

## `eryx-vfs` - [0.8.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.7.2...eryx-vfs-v0.8.0) - 2026-09-19

### Other
- *(vfs)* replace cap-std with cap-primitives ([#436](https://github.com/eryx-org/eryx/pull/436))

## `eryx-runtime` - [0.8.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.7.2...eryx-runtime-v0.8.0) - 2026-09-19

### Added
- [**breaking**] change stdout/stderr from string to bytes across the full stack ([#448](https://github.com/eryx-org/eryx/pull/448))

## `eryx-precompile` - [0.7.2](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.7.1...eryx-precompile-v0.7.2) - 2026-09-11

### Other
- *(preinit)* bake callback declarations into the snapshot ([#423](https://github.com/eryx-org/eryx/pull/423))

## `eryx` - [0.7.2](https://github.com/eryx-org/eryx/compare/eryx-v0.7.1...eryx-v0.7.2) - 2026-09-11

### Added
- make trace collection easy to disable (docs + ERYX_PROFILE_TRACE) ([#424](https://github.com/eryx-org/eryx/pull/424))

### Other
- *(preinit)* bake callback declarations into the snapshot ([#423](https://github.com/eryx-org/eryx/pull/423))
- *(runtime)* only reinstall callbacks when their declarations change ([#416](https://github.com/eryx-org/eryx/pull/416))

## `eryx-runtime` - [0.7.2](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.7.1...eryx-runtime-v0.7.2) - 2026-09-11

### Other
- *(preinit)* bake callback declarations into the snapshot ([#423](https://github.com/eryx-org/eryx/pull/423))
- *(runtime)* call the execute helpers directly instead of compiling snippets ([#418](https://github.com/eryx-org/eryx/pull/418))
- *(runtime)* only reinstall callbacks when their declarations change ([#416](https://github.com/eryx-org/eryx/pull/416))
- *(deps)* update wasmtime & wasm-tools ([#414](https://github.com/eryx-org/eryx/pull/414))

## `eryx-precompile` - [0.7.1](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.7.0...eryx-precompile-v0.7.1) - 2026-09-10

### Fixed
- normalize native extension dlopen paths on Windows ([#405](https://github.com/eryx-org/eryx/pull/405))

## `eryx` - [0.7.1](https://github.com/eryx-org/eryx/compare/eryx-v0.7.0...eryx-v0.7.1) - 2026-09-10

### Fixed
- normalize native extension dlopen paths on Windows ([#405](https://github.com/eryx-org/eryx/pull/405))

## `eryx-precompile` - [0.7.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.6.0...eryx-precompile-v0.7.0) - 2026-09-10

### Added
- *(preinit)* add setup_code parameter to pre-initialize arbitrary state ([#397](https://github.com/eryx-org/eryx/pull/397))
- [**breaking**] add a Baseline CPU feature level and seal CpuFeatureLevel ([#361](https://github.com/eryx-org/eryx/pull/361))

### Fixed
- pin the compilation target so CPU features cannot leak from the build host ([#360](https://github.com/eryx-org/eryx/pull/360))

### Other
- *(deps)* update dependency cargo:cargo-rail to v0.25.0 ([#367](https://github.com/eryx-org/eryx/pull/367))
- *(deps)* update dependency cargo:cargo-rail to v0.22.1 ([#365](https://github.com/eryx-org/eryx/pull/365))

## `eryx` - [0.7.0](https://github.com/eryx-org/eryx/compare/eryx-v0.6.0...eryx-v0.7.0) - 2026-09-10

### Added
- add factory-backed Python sessions ([#392](https://github.com/eryx-org/eryx/pull/392))
- *(preinit)* add setup_code parameter to pre-initialize arbitrary state ([#397](https://github.com/eryx-org/eryx/pull/397))
- [**breaking**] add a Baseline CPU feature level and seal CpuFeatureLevel ([#361](https://github.com/eryx-org/eryx/pull/361))

### Fixed
- share the epoch ticker across executions ([#349](https://github.com/eryx-org/eryx/pull/349))
- pin the compilation target so CPU features cannot leak from the build host ([#360](https://github.com/eryx-org/eryx/pull/360))

### Other
- *(deps)* update dependency cargo:cargo-rail to v0.25.0 ([#367](https://github.com/eryx-org/eryx/pull/367))
- *(deps)* update wasmtime & wasm-tools ([#343](https://github.com/eryx-org/eryx/pull/343))
- *(deps)* update dependency rust to 1.98 ([#354](https://github.com/eryx-org/eryx/pull/354))

## `eryx-vfs` - [0.7.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.6.0...eryx-vfs-v0.7.0) - 2026-09-10

### Other
- *(deps)* update dependency cargo:cargo-rail to v0.25.0 ([#367](https://github.com/eryx-org/eryx/pull/367))
- *(deps)* update wasmtime & wasm-tools ([#343](https://github.com/eryx-org/eryx/pull/343))

## `eryx-runtime` - [0.7.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.6.0...eryx-runtime-v0.7.0) - 2026-09-10

### Added
- *(preinit)* add setup_code parameter to pre-initialize arbitrary state ([#397](https://github.com/eryx-org/eryx/pull/397))

### Other
- *(deps)* update wasmtime & wasm-tools ([#370](https://github.com/eryx-org/eryx/pull/370))
- *(deps)* update wasmtime & wasm-tools ([#343](https://github.com/eryx-org/eryx/pull/343))

## `eryx-macros` - [0.7.0](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.6.0...eryx-macros-v0.7.0) - 2026-09-10

### Other
- *(deps)* update dependency cargo:cargo-rail to v0.25.0 ([#367](https://github.com/eryx-org/eryx/pull/367))

## `eryx-precompile` - [0.6.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.5.0...eryx-precompile-v0.6.0) - 2026-08-19

### Other
- update Cargo.toml dependencies

## `eryx` - [0.6.0](https://github.com/eryx-org/eryx/compare/eryx-v0.5.0...eryx-v0.6.0) - 2026-08-19

### Security
- Bound guest-controlled host allocations so sandboxed code cannot exhaust host
  memory: socket reads (`tcp_read`, `tls_read`) no longer size a host buffer from
  the length the guest asked for, and the in-memory virtual filesystem is capped
  (see `eryx-vfs` below). VFS and socket buffers live in host memory, outside
  `max_memory_bytes`, so no configuration previously bounded them.
  (GHSA-8h84-8vq7-j6cv)

### Changed
- [**breaking**] `ResourceLimits` is now `#[non_exhaustive]`, so it can no longer
  be built with a struct literal - including with `..Default::default()`. Use the
  builder instead: `ResourceLimits::default().with_max_fuel(n)`. Its fields stay
  public, so reading them, and mutating them on a value you own, both still work.
- `ResourceLimits::max_vfs_bytes` caps the in-memory virtual filesystem, with
  `ResourceLimits::unlimited()` and a `with_*` setter per limit

### Added
- cache pre-compiled bytes in InstancePreCache ([#265](https://github.com/eryx-org/eryx/pull/265))

### Other
- *(net)* don't link private MAX_READ_LEN from public docs ([#339](https://github.com/eryx-org/eryx/pull/339))
- Merge commit from fork
- *(eryx)* share pre-init bytes across preinit tests ([#338](https://github.com/eryx-org/eryx/pull/338))
- Optimize callback setup and Python tracing ([#270](https://github.com/eryx-org/eryx/pull/270))
- *(deps)* update wasmtime & wasm-tools ([#279](https://github.com/eryx-org/eryx/pull/279))
- *(deps)* update rust crate rand to 0.10 ([#302](https://github.com/eryx-org/eryx/pull/302))
- *(deps)* update rust crate rcgen to 0.14 ([#304](https://github.com/eryx-org/eryx/pull/304))
- fix main after cargo-rail 0.21 bump, and stop mise.toml changes skipping CI ([#309](https://github.com/eryx-org/eryx/pull/309))

## `eryx-vfs` - [0.6.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.5.0...eryx-vfs-v0.6.0) - 2026-08-19

### Security
- `InMemoryStorage` now bounds the total bytes it will hold, 64 MiB by default
  (`DEFAULT_MAX_BYTES`). File contents live in host memory and the guest chooses
  its own write offsets, so a single small write at a large offset could make the
  host allocate arbitrarily much. Writes past the budget fail with the new
  `VfsError::QuotaExceeded`, which guests see as `ENOSPC`; sparse writes inside
  the budget still work. Stream reads are capped per call. Configure with
  `InMemoryStorage::with_max_bytes`. (GHSA-8h84-8vq7-j6cv)

### Added
- *(eryx-vfs)* snapshot / restore / clone for InMemoryStorage ([#259](https://github.com/eryx-org/eryx/pull/259))

### Other
- Merge commit from fork
- *(deps)* update wasmtime & wasm-tools ([#279](https://github.com/eryx-org/eryx/pull/279))

## `eryx-runtime` - [0.6.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.5.0...eryx-runtime-v0.6.0) - 2026-08-19

### Other
- Optimize callback setup and Python tracing ([#270](https://github.com/eryx-org/eryx/pull/270))
- *(eryx-python)* guard CPython finalization; fix libs/ rebuild tracking ([#333](https://github.com/eryx-org/eryx/pull/333))
- *(deps)* update wasmtime & wasm-tools ([#331](https://github.com/eryx-org/eryx/pull/331))
- *(deps)* update wasmtime & wasm-tools ([#279](https://github.com/eryx-org/eryx/pull/279))

## `eryx-macros` - [0.6.0](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.5.0...eryx-macros-v0.6.0) - 2026-08-19

### Other
- *(deps)* update rust crate syn to v3 ([#323](https://github.com/eryx-org/eryx/pull/323))
- fix main after cargo-rail 0.21 bump, and stop mise.toml changes skipping CI ([#309](https://github.com/eryx-org/eryx/pull/309))

## `eryx-precompile` - [0.5.0](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.10...eryx-precompile-v0.5.0) - 2026-06-16

### Other
- update Cargo.lock dependencies

## `eryx` - [0.5.0](https://github.com/eryx-org/eryx/compare/eryx-v0.4.10...eryx-v0.5.0) - 2026-06-16

### Added
- callback suspension via fuel-poison halt ([#230](https://github.com/eryx-org/eryx/pull/230))
- callback-result replay for the sandbox + gRPC API ([#228](https://github.com/eryx-org/eryx/pull/228))
- mark result/output structs and error enums #[non_exhaustive] ([#238](https://github.com/eryx-org/eryx/pull/238))
- capture a structured result from executed scripts ([#229](https://github.com/eryx-org/eryx/pull/229))

### Fixed
- *(net)* honor Python socket timeouts, capped by the host config ([#240](https://github.com/eryx-org/eryx/pull/240))
- *(server)* surface captured output on the execution error path ([#239](https://github.com/eryx-org/eryx/pull/239))

### Other
- add callback replay & suspension guide ([#243](https://github.com/eryx-org/eryx/pull/243))

## `eryx-vfs` - [0.5.0](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.10...eryx-vfs-v0.5.0) - 2026-06-16

### Added
- mark result/output structs and error enums #[non_exhaustive] ([#238](https://github.com/eryx-org/eryx/pull/238))

## `eryx-runtime` - [0.5.0](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.10...eryx-runtime-v0.5.0) - 2026-06-16

### Added
- mark result/output structs and error enums #[non_exhaustive] ([#238](https://github.com/eryx-org/eryx/pull/238))
- capture a structured result from executed scripts ([#229](https://github.com/eryx-org/eryx/pull/229))

### Fixed
- *(net)* treat _GLOBAL_DEFAULT_TIMEOUT sentinel as no timeout ([#247](https://github.com/eryx-org/eryx/pull/247))
- *(net)* honor Python socket timeouts, capped by the host config ([#240](https://github.com/eryx-org/eryx/pull/240))
- expose SSLSocket._sslobj so httpx HTTPS works in the sandbox ([#236](https://github.com/eryx-org/eryx/pull/236))

### Other
- regenerate prebuilt runtime snapshot with ssl VERIFY_X509_PARTIAL_CHAIN ([#233](https://github.com/eryx-org/eryx/pull/233))

## `eryx-precompile` - [0.4.10](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.9...eryx-precompile-v0.4.10) - 2026-06-03

### Other
- update Cargo.lock dependencies

## `eryx` - [0.4.10](https://github.com/eryx-org/eryx/compare/eryx-v0.4.9...eryx-v0.4.10) - 2026-06-03

### Other
- update Cargo.toml dependencies

## `eryx-vfs` - [0.4.10](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.9...eryx-vfs-v0.4.10) - 2026-06-03

### Other
- update Cargo.toml dependencies

## `eryx-runtime` - [0.4.10](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.9...eryx-runtime-v0.4.10) - 2026-06-03

### Other
- update Cargo.toml dependencies

## `eryx-precompile` - [0.4.9](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.8...eryx-precompile-v0.4.9) - 2026-04-15

### Other
- update Cargo.lock dependencies

## `eryx` - [0.4.9](https://github.com/eryx-org/eryx/compare/eryx-v0.4.8...eryx-v0.4.9) - 2026-04-15

### Added
- *(net)* add explicit allow_all_hosts field to NetworkConfig ([#190](https://github.com/eryx-org/eryx/pull/190))

### Fixed
- *(pool)* use builder config instead of hardcoding embedded runtime ([#198](https://github.com/eryx-org/eryx/pull/198))

### Other
- *(deps)* bump wasmtime from 41 to 43 ([#193](https://github.com/eryx-org/eryx/pull/193))

## `eryx-vfs` - [0.4.9](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.8...eryx-vfs-v0.4.9) - 2026-04-15

### Other
- *(deps)* bump wasmtime from 41 to 43 ([#193](https://github.com/eryx-org/eryx/pull/193))

## `eryx-runtime` - [0.4.9](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.8...eryx-runtime-v0.4.9) - 2026-04-15

### Other
- *(deps)* bump wasmtime from 41 to 43 ([#193](https://github.com/eryx-org/eryx/pull/193))

## `eryx-precompile` - [0.4.8](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.7...eryx-precompile-v0.4.8) - 2026-03-27

### Other
- update Cargo.lock dependencies

## `eryx` - [0.4.8](https://github.com/eryx-org/eryx/compare/eryx-v0.4.7...eryx-v0.4.8) - 2026-03-27

### Added
- *(server)* wire secrets support into gRPC service ([#179](https://github.com/eryx-org/eryx/pull/179))

## `eryx-precompile` - [0.4.7](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.6...eryx-precompile-v0.4.7) - 2026-03-13

### Added
- add gRPC server for sandboxed Python execution ([#158](https://github.com/eryx-org/eryx/pull/158))

### Other
- add eryx-precompile guide ([#163](https://github.com/eryx-org/eryx/pull/163))

## `eryx` - [0.4.7](https://github.com/eryx-org/eryx/compare/eryx-v0.4.6...eryx-v0.4.7) - 2026-03-13

### Added
- add `embedded-stdlib` feature for lightweight stdlib embedding ([#162](https://github.com/eryx-org/eryx/pull/162))
- add gRPC server for sandboxed Python execution ([#158](https://github.com/eryx-org/eryx/pull/158))

### Fixed
- support all-features docs.rs builds with WASM placeholders ([#154](https://github.com/eryx-org/eryx/pull/154))
- callback bugs for optional args and error reporting ([#153](https://github.com/eryx-org/eryx/pull/153))
- handle complex WIT types in EryxCall trait implementation ([#152](https://github.com/eryx-org/eryx/pull/152))

## `eryx-vfs` - [0.4.7](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.6...eryx-vfs-v0.4.7) - 2026-03-13

### Added
- add gRPC server for sandboxed Python execution ([#158](https://github.com/eryx-org/eryx/pull/158))

## `eryx-runtime` - [0.4.7](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.6...eryx-runtime-v0.4.7) - 2026-03-13

### Fixed
- support all-features docs.rs builds with WASM placeholders ([#154](https://github.com/eryx-org/eryx/pull/154))
- callback bugs for optional args and error reporting ([#153](https://github.com/eryx-org/eryx/pull/153))

## `eryx-precompile` - [0.4.6](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.5...eryx-precompile-v0.4.6) - 2026-02-25

### Other
- update Cargo.lock dependencies

## `eryx` - [0.4.6](https://github.com/eryx-org/eryx/compare/eryx-v0.4.5...eryx-v0.4.6) - 2026-02-25

### Other
- update callback examples to use #[callback] macro ([#142](https://github.com/eryx-org/eryx/pull/142))

## `eryx` - [0.4.5](https://github.com/eryx-org/eryx/compare/eryx-v0.4.4...eryx-v0.4.5) - 2026-02-11

### Fixed
- use bracket syntax for MCP tool names in list_tools output ([#136](https://github.com/eryx-org/eryx/pull/136))

### Added
- *(demo)* add in-browser native WASM extension support ([#134](https://github.com/eryx-org/eryx/pull/134))

### Other
- mark eryx-linker-wasm as publish = false ([#138](https://github.com/eryx-org/eryx/pull/138))
- tweak some wording and silly claims ([#135](https://github.com/eryx-org/eryx/pull/135))
- bump cryptography in /crates/eryx-python ([#137](https://github.com/eryx-org/eryx/pull/137))

## `eryx-macros` - [0.4.5](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.4.4...eryx-macros-v0.4.5) - 2026-02-11

### Other
- mark eryx-linker-wasm as publish = false ([#138](https://github.com/eryx-org/eryx/pull/138))

## `eryx-runtime` - [0.4.5](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.4...eryx-runtime-v0.4.5) - 2026-02-11

### Other
- mark eryx-linker-wasm as publish = false ([#138](https://github.com/eryx-org/eryx/pull/138))

## `eryx-vfs` - [0.4.5](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.4...eryx-vfs-v0.4.5) - 2026-02-11

### Other
- mark eryx-linker-wasm as publish = false ([#138](https://github.com/eryx-org/eryx/pull/138))

## `eryx-precompile` - [0.4.5](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.4...eryx-precompile-v0.4.5) - 2026-02-11

### Other
- mark eryx-linker-wasm as publish = false ([#138](https://github.com/eryx-org/eryx/pull/138))

## `eryx-precompile` - [0.4.4](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.3...eryx-precompile-v0.4.4) - 2026-02-10

### Other
- update Cargo.lock dependencies

## `eryx` - [0.4.4](https://github.com/eryx-org/eryx/compare/eryx-v0.4.2...eryx-v0.4.4) - 2026-02-10

### Other
- release v0.4.3 ([#127](https://github.com/eryx-org/eryx/pull/127))

## `eryx-vfs` - [0.4.4](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.2...eryx-vfs-v0.4.4) - 2026-02-10

### Fixed
- Windows symlink support + add Windows CI check ([#126](https://github.com/eryx-org/eryx/pull/126))

## `eryx-vfs` - [0.4.3](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.2...eryx-vfs-v0.4.3) - 2026-02-10

### Fixed
- Windows symlink support + add Windows CI check ([#126](https://github.com/eryx-org/eryx/pull/126))

## `eryx-precompile` - [0.4.2](https://github.com/eryx-org/eryx/compare/eryx-precompile-v0.4.1...eryx-precompile-v0.4.2) - 2026-02-10

### Fixed
- tidy crate doc comments to trigger release ([#118](https://github.com/eryx-org/eryx/pull/118))

## `eryx` - [0.4.2](https://github.com/eryx-org/eryx/compare/eryx-v0.4.1...eryx-v0.4.2) - 2026-02-10

### Fixed
- tidy crate doc comments to trigger release ([#118](https://github.com/eryx-org/eryx/pull/118))

## `eryx-vfs` - [0.4.2](https://github.com/eryx-org/eryx/compare/eryx-vfs-v0.4.1...eryx-vfs-v0.4.2) - 2026-02-10

### Fixed
- tidy crate doc comments to trigger release ([#118](https://github.com/eryx-org/eryx/pull/118))

## `eryx-runtime` - [0.4.2](https://github.com/eryx-org/eryx/compare/eryx-runtime-v0.4.1...eryx-runtime-v0.4.2) - 2026-02-10

### Fixed
- *(ci)* unpin nightly and clean stale build-std artifacts on rebuild ([#122](https://github.com/eryx-org/eryx/pull/122))
- pin nightly toolchain to 2026-01-31 for WASM build ([#121](https://github.com/eryx-org/eryx/pull/121))
- tidy crate doc comments to trigger release ([#118](https://github.com/eryx-org/eryx/pull/118))

## `eryx-macros` - [0.4.2](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.4.1...eryx-macros-v0.4.2) - 2026-02-10

### Fixed
- tidy crate doc comments to trigger release ([#118](https://github.com/eryx-org/eryx/pull/118))

## `eryx` - [0.4.1](https://github.com/eryx-org/eryx/compare/eryx-v0.4.0...eryx-v0.4.1) - 2026-02-09

### Other
- add publish_no_verify for eryx-macros and eryx ([#107](https://github.com/eryx-org/eryx/pull/107))

## `eryx-macros` - [0.4.1](https://github.com/eryx-org/eryx/compare/eryx-macros-v0.4.0...eryx-macros-v0.4.1) - 2026-02-09

### Other
- add publish_no_verify for eryx-macros and eryx ([#107](https://github.com/eryx-org/eryx/pull/107))

## `eryx-vfs` - [0.4.0](https://github.com/eryx-org/eryx/releases/tag/eryx-vfs-v0.4.0) - 2026-02-09

### Added
- *(volumes)* add host filesystem volume mounts ([#71](https://github.com/eryx-org/eryx/pull/71))
- *(secrets)* implement placeholder substitution for secure secrets management ([#58](https://github.com/eryx-org/eryx/pull/58))
- *(vfs)* implement stream support for read_via_stream/write_via_stream/append_via_stream
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor

### Fixed
- *(eryx-vfs)* fix Windows build by downgrading cap-std to v3
- *(eryx-vfs)* enable cap_std_impls feature for Windows support
- *(vfs)* improve thread safety, configurability, and documentation
- *(vfs)* improve thread safety and reduce code duplication
- resolve CI failures (clippy lints and Python VfsStorage export)

### Other
- Bump wasmtime

## `eryx-runtime` - [0.4.0](https://github.com/eryx-org/eryx/releases/tag/eryx-runtime-v0.4.0) - 2026-02-09

### Added
- *(python)* add MCP server integration via rmcp ([#85](https://github.com/eryx-org/eryx/pull/85))
- *(output)* stream stdout/stderr in real-time via report-output WIT import ([#75](https://github.com/eryx-org/eryx/pull/75))
- *(python)* switch snapshot/restore serialization from pickle to dill ([#72](https://github.com/eryx-org/eryx/pull/72))
- add SQLite3 support
- add stderr capture and streaming
- *(preinit)* add network interface stubs for TCP/TLS
- *(net)* add TCP and TLS networking with eryx:net package
- *(tls)* add TLS WIT interface and host implementation
- split preinit feature from native-extensions
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- add cargo-rail and cargo-all-features support
- simplify feature flags from 6 to 2
- *(eryx)* add pre-initialization support for native extensions
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation

### Fixed
- *(build)* prefer prebuilt artifacts over cached OUT_DIR
- *(net)* align TCP/TLS shims with sync WIT imports using fiber-based async
- *(net)* use fiber-based async for TCP/TLS socket shims
- update preinit vs native-extensions feature handling
- benchmark clippy lints and asyncio.run issue
- add build-native-extensions task for --all-features commands
- format code with cargo fmt
- *(ci)* auto-decompress libs in build.rs
- *(ci)* use POSIX-compatible test syntax in ensure-wasi-sdk
- resolve session state and link test failures

### Other
- *(runtime)* replace component-init-transform with wasmtime-wizer ([#78](https://github.com/eryx-org/eryx/pull/78))
- add prebuilt late-linking artifacts for CI ([#50](https://github.com/eryx-org/eryx/pull/50))
- Revert "feat(tls): add TLS WIT interface and host implementation"
- Document sandbox Python version
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

## `eryx-macros` - [0.4.0](https://github.com/eryx-org/eryx/releases/tag/eryx-macros-v0.4.0) - 2026-02-09

### Added
- *(macros)* add #[callback] proc macro for simplified callback definitions ([#64](https://github.com/eryx-org/eryx/pull/64))

## `eryx-wasm-runtime` - [0.3.0](https://github.com/eryx-org/eryx/releases/tag/eryx-wasm-runtime-v0.3.0) - 2026-02-01

### Added
- add stderr capture and streaming
- *(socket-shim)* implement deferred close pattern for http.client compatibility
- *(net)* add TCP and TLS networking with eryx:net package
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
- clear async result dicts per execution to fix flaky tests
- move _eryx_async_import_results init to ERYX_EXEC_INFRASTRUCTURE
- *(async)* key callback results by subtask ID for concurrent safety
- improve callback error handling and document async limitations
- correct record field push order for wit-dylib LIFO stack
- *(ci)* force rebuild all eryx-wasm-runtime sources to prevent stale cache
- *(net)* align TCP/TLS shims with sync WIT imports using fiber-based async
- *(net)* use fiber-based async for TCP/TLS socket shims
- check ERYX_PYTHON_STDLIB env var in runtime_test.rs
- rename invoke() parameter to avoid conflicts with callback kwargs
- *(docs)* escape generic type in rustdoc comment
- *(ci)* auto-decompress libs in build.rs
- resolve session state and link test failures

### Other
- fix formatting issues
- Fix doc test issues
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

## `eryx-python` - [0.3.0](https://github.com/eryx-org/eryx/releases/tag/eryx-python-v0.3.0) - 2026-02-01

### Added
- add fuel-based instruction tracking and limiting
- add SQLite3 support
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor
- add stderr capture and streaming
- *(eryx-python)* add NetConfig for network configuration
- add epoch-based execution timeout support
- *(eryx-python)* add PreInitializedRuntime for fast sandbox creation
- *(eryx-python)* add native extensions and package loading support
- *(eryx-python)* add Phase 1 PyO3 Python bindings (MVP)

### Fixed
- clear async result dicts per execution to fix flaky tests
- *(async)* key callback results by subtask ID for concurrent safety
- *(session)* track execution duration and callback invocations in ExecutionOutput
- resolve CI failures (clippy lints and Python VfsStorage export)
- handle Error::Cancelled in Python bindings and fix formatting
- *(ci)* force rebuild all eryx-wasm-runtime sources to prevent stale cache
- *(net)* use fiber-based async for TCP/TLS socket shims

### Other
- remove inaccurate notice about Python 3.14 from READMEs
- use typed error variants instead of string matching
- add fuel exhaustion tests and improve error message
- Merge pull request #30 from eryx-org/sqlite3
- *(python)* add Session and VFS documentation
- *(python)* remove all pytest.skip calls, use WASI wheel for markupsafe
- *(python)* add HTTP library integration tests with test fixtures
- *(eryx-python)* update uv.lock for test dependencies
- *(eryx-python)* add cryptography dev dependency for HTTPS tests
- Document sandbox Python version
- *(pyeryx)* use version from Cargo.toml
- *(pyeryx)* rename PreInitializedRuntime to SandboxFactory and simplify Sandbox API
- Include notes on PreInitializedRuntime in Python README
- Fix links to nonexistent github repo
- Use 'pyeryx' for Python package
- Run mise unify

## `eryx-precompile` - [0.3.0](https://github.com/eryx-org/eryx/releases/tag/eryx-precompile-v0.3.0) - 2026-02-01

### Added
- add eryx-precompile CLI binary

### Fixed
- replace expect() with ok_or_else() for clippy
- resolve CI failures for eryx-precompile

## `eryx` - [0.3.0](https://github.com/eryx-org/eryx/compare/eryx-v0.1.0...eryx-v0.3.0) - 2026-02-01

### Added
- add tracing instrumentation to key eryx functions
- add eryx-precompile CLI binary
- add fuel-based instruction tracking and limiting

### Fixed
- separate precompile outputs for preinit vs non-preinit variants

### Other
- Run mise run fmt
- Merge pull request #42 from eryx-org/fix/typed-error-variants
- use typed error variants instead of string matching
- add fuel exhaustion tests and improve error message

## `eryx-vfs` - [0.3.0](https://github.com/eryx-org/eryx/releases/tag/eryx-vfs-v0.3.0) - 2026-02-01

### Added
- *(vfs)* implement stream support for read_via_stream/write_via_stream/append_via_stream
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor

### Fixed
- *(eryx-vfs)* fix Windows build by downgrading cap-std to v3
- *(eryx-vfs)* enable cap_std_impls feature for Windows support
- *(vfs)* improve thread safety, configurability, and documentation
- *(vfs)* improve thread safety and reduce code duplication
- resolve CI failures (clippy lints and Python VfsStorage export)

### Other
- Bump wasmtime

## `eryx-runtime` - [0.3.0](https://github.com/eryx-org/eryx/releases/tag/eryx-runtime-v0.3.0) - 2026-02-01

### Added
- add SQLite3 support
- add stderr capture and streaming
- *(preinit)* add network interface stubs for TCP/TLS
- *(net)* add TCP and TLS networking with eryx:net package
- *(tls)* add TLS WIT interface and host implementation
- split preinit feature from native-extensions
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- add cargo-rail and cargo-all-features support
- simplify feature flags from 6 to 2
- *(eryx)* add pre-initialization support for native extensions
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation

### Fixed
- *(build)* prefer prebuilt artifacts over cached OUT_DIR
- *(net)* align TCP/TLS shims with sync WIT imports using fiber-based async
- *(net)* use fiber-based async for TCP/TLS socket shims
- update preinit vs native-extensions feature handling
- benchmark clippy lints and asyncio.run issue
- add build-native-extensions task for --all-features commands
- format code with cargo fmt
- *(ci)* auto-decompress libs in build.rs
- *(ci)* use POSIX-compatible test syntax in ensure-wasi-sdk
- resolve session state and link test failures

### Other
- Revert "feat(tls): add TLS WIT interface and host implementation"
- Document sandbox Python version
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

## `eryx-wasm-runtime` - [0.2.0](https://github.com/eryx-org/eryx/releases/tag/eryx-wasm-runtime-v0.2.0) - 2026-01-30

### Added
- add stderr capture and streaming
- *(socket-shim)* implement deferred close pattern for http.client compatibility
- *(net)* add TCP and TLS networking with eryx:net package
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
- improve callback error handling and document async limitations
- correct record field push order for wit-dylib LIFO stack
- *(ci)* force rebuild all eryx-wasm-runtime sources to prevent stale cache
- *(net)* align TCP/TLS shims with sync WIT imports using fiber-based async
- *(net)* use fiber-based async for TCP/TLS socket shims
- check ERYX_PYTHON_STDLIB env var in runtime_test.rs
- rename invoke() parameter to avoid conflicts with callback kwargs
- *(docs)* escape generic type in rustdoc comment
- *(ci)* auto-decompress libs in build.rs
- resolve session state and link test failures

### Other
- fix formatting issues
- Fix doc test issues
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

## `eryx-python` - [0.2.0](https://github.com/eryx-org/eryx/releases/tag/eryx-python-v0.2.0) - 2026-01-30

### Added
- add SQLite3 support
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor
- add stderr capture and streaming
- *(eryx-python)* add NetConfig for network configuration
- add epoch-based execution timeout support
- *(eryx-python)* add PreInitializedRuntime for fast sandbox creation
- *(eryx-python)* add native extensions and package loading support
- *(eryx-python)* add Phase 1 PyO3 Python bindings (MVP)

### Fixed
- *(session)* track execution duration and callback invocations in ExecutionOutput
- resolve CI failures (clippy lints and Python VfsStorage export)
- handle Error::Cancelled in Python bindings and fix formatting
- *(ci)* force rebuild all eryx-wasm-runtime sources to prevent stale cache
- *(net)* use fiber-based async for TCP/TLS socket shims

### Other
- Merge pull request #30 from eryx-org/sqlite3
- *(python)* add Session and VFS documentation
- *(python)* remove all pytest.skip calls, use WASI wheel for markupsafe
- *(python)* add HTTP library integration tests with test fixtures
- *(eryx-python)* update uv.lock for test dependencies
- *(eryx-python)* add cryptography dev dependency for HTTPS tests
- Document sandbox Python version
- *(pyeryx)* use version from Cargo.toml
- *(pyeryx)* rename PreInitializedRuntime to SandboxFactory and simplify Sandbox API
- Include notes on PreInitializedRuntime in Python README
- Fix links to nonexistent github repo
- Use 'pyeryx' for Python package
- Run mise unify

## `eryx` - [0.2.0](https://github.com/eryx-org/eryx/compare/eryx-v0.1.0...eryx-v0.2.0) - 2026-01-30

### Added
- add SQLite3 support
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor
- add execution cancellation support with ExecutionHandle
- add stderr capture and streaming
- *(embedded)* add content hash to runtime cache filename
- *(net)* add TCP and TLS networking with eryx:net package
- *(tls)* add TLS WIT interface and host implementation

### Fixed
- *(session)* track execution duration and callback invocations in ExecutionOutput
- *(vfs)* improve thread safety, configurability, and documentation
- *(vfs)* improve thread safety and reduce code duplication
- additional clippy lints in eryx crate
- increase cancel delay in flaky test to 10 seconds
- use reachable epoch deadline for cancellation-only execution
- handle Error::Cancelled in Python bindings and fix formatting
- *(wasm)* mount first site-packages at /site-packages for preinit compatibility
- *(net)* use fiber-based async for TCP/TLS socket shims
- *(lint)* resolve clippy warnings in net.rs and wasm.rs

### Other
- Merge pull request #30 from eryx-org/sqlite3
- *(vfs)* add additional security tests
- *(security)* add socket bypass security tests
- Cargo fmt
- Merge branch 'main' into feature/sandbox-pooling
- Fix rebase issue
- fix formatting issues
- Revert "feat(tls): add TLS WIT interface and host implementation"

## `eryx-vfs` - [0.2.0](https://github.com/eryx-org/eryx/releases/tag/eryx-vfs-v0.2.0) - 2026-01-30

### Added
- *(vfs)* implement stream support for read_via_stream/write_via_stream/append_via_stream
- *(python)* add Session class with VFS support
- *(vfs)* implement hybrid VFS for SessionExecutor

### Fixed
- *(vfs)* improve thread safety, configurability, and documentation
- *(vfs)* improve thread safety and reduce code duplication
- resolve CI failures (clippy lints and Python VfsStorage export)

### Other
- Bump wasmtime

## `eryx-runtime` - [0.2.0](https://github.com/eryx-org/eryx/releases/tag/eryx-runtime-v0.2.0) - 2026-01-30

### Added
- add SQLite3 support
- add stderr capture and streaming
- *(preinit)* add network interface stubs for TCP/TLS
- *(net)* add TCP and TLS networking with eryx:net package
- *(tls)* add TLS WIT interface and host implementation
- split preinit feature from native-extensions
- *(preinit)* add finalize-preinit export to fix WASI handle invalidation
- add cargo-rail and cargo-all-features support
- simplify feature flags from 6 to 2
- *(eryx)* add pre-initialization support for native extensions
- Add native Python extension support via late-linking
- *(eryx-wasm-runtime)* complete async callback implementation

### Fixed
- *(build)* prefer prebuilt artifacts over cached OUT_DIR
- *(net)* align TCP/TLS shims with sync WIT imports using fiber-based async
- *(net)* use fiber-based async for TCP/TLS socket shims
- update preinit vs native-extensions feature handling
- benchmark clippy lints and asyncio.run issue
- add build-native-extensions task for --all-features commands
- format code with cargo fmt
- *(ci)* auto-decompress libs in build.rs
- *(ci)* use POSIX-compatible test syntax in ensure-wasi-sdk
- resolve session state and link test failures

### Other
- Revert "feat(tls): add TLS WIT interface and host implementation"
- Document sandbox Python version
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
