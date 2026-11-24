# gRPC Server

Eryx can run as a standalone **gRPC server** (`eryx-server`) that executes Python in a sandbox on behalf of remote clients. A pool of warm sandboxes is shared across requests, and a single bidirectional streaming RPC carries everything a request needs: [callbacks](./callbacks.md), real-time [output streaming](./output-streaming.md), execution tracing, [secrets](./secrets.md), [networking](./networking.md), [session state](./sessions.md), the structured result value, and [callback replay & suspension](./callback-replay.md).

It's the backend for hosted, multi-tenant deployments where sandbox execution runs as a remote service. If you only need to embed the sandbox in a single process, use the [Rust API](../api/rust.md) directly; if you want a tool for an AI assistant over stdio, see the [MCP Server](./mcp-server.md). The gRPC server is for remote, multi-tenant, networked execution.

> The protobuf service definition is the source of truth for the API: [`crates/eryx-server/proto/eryx/v1/eryx.proto`](https://github.com/eryx-org/eryx/blob/main/crates/eryx-server/proto/eryx/v1/eryx.proto). The fields and messages described below are documented inline there.

## Running the server

The easiest way to run the server is the published Docker image, [`sd2k/eryx-server`](https://hub.docker.com/r/sd2k/eryx-server):

```bash
docker run -p 50051:50051 sd2k/eryx-server
```

The plain tags (`latest`, `X.Y.Z`, `X.Y`) are the image with every guest language: Python, JavaScript and shell. For a smaller Python-only image, add `-python` to any tag (`latest-python`, `X.Y.Z-python`); a request for another language then fails with `INVALID_ARGUMENT`.

The image sets `ENTRYPOINT ["eryx-server"]` and listens on `[::]:50051` (metrics on `9090`); it's built from `crates/eryx-server/Dockerfile`. It runs as the unprivileged user `eryx` (UID/GID `10001`), so it works under Kubernetes' `runAsNonRoot`. The embedded runtime and stdlib are extracted at startup to `$XDG_CACHE_HOME/eryx` or `~/.cache/eryx`, falling back to `$TMPDIR/eryx-<uid>` when those can't be created. With a read-only root filesystem, mount a writable `/tmp` (or set `XDG_CACHE_HOME` to a writable directory).

To build from source instead, the server is the `eryx-server` binary in the `eryx-server` crate. Build it with the `embedded` and `preinit` features so the runtime and stdlib are baked in:

```bash
cargo build --release -p eryx-server --features eryx/embedded,eryx/preinit
./target/release/eryx-server
```

Every flag also has an environment variable, so the server is easy to configure in a container:

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--listen-addr` | `ERYX_LISTEN_ADDR` | `[::1]:50051` | Address the gRPC service binds to. |
| `--pool-max-size` | `ERYX_POOL_MAX_SIZE` | `10` | Maximum sandboxes in the pool. |
| `--pool-min-idle` | `ERYX_POOL_MIN_IDLE` | `1` | Idle sandboxes kept warm for low-latency acquisition. |
| `--pool-acquire-timeout-secs` | `ERYX_POOL_ACQUIRE_TIMEOUT_SECS` | `30` | How long a request waits for a free sandbox before failing with `UNAVAILABLE`. |
| `--pool-idle-timeout-secs` | `ERYX_POOL_IDLE_TIMEOUT_SECS` | `300` | How long a sandbox beyond `--pool-min-idle` may sit idle before it is evicted. |
| `--max-message-size` | `ERYX_MAX_MESSAGE_SIZE` | `33554432` (32 MiB) | Largest gRPC message accepted or sent, in bytes. Clients must raise their own limits to match. |
| `--max-concurrent-streams` | `ERYX_MAX_CONCURRENT_STREAMS` | `200` | Concurrent RPCs per client connection. Further RPCs on that connection wait for a free stream. |
| `--http2-keepalive-interval-secs` | `ERYX_HTTP2_KEEPALIVE_INTERVAL_SECS` | `60` | Interval between HTTP/2 keepalive pings to each client; `0` disables them. |
| `--http2-keepalive-timeout-secs` | `ERYX_HTTP2_KEEPALIVE_TIMEOUT_SECS` | `20` | How long to wait for a ping acknowledgement before closing the connection. |
| `--metrics-addr` | `ERYX_METRICS_ADDR` | `0.0.0.0:9090` | Address of the Prometheus `/metrics` endpoint. |
| `--runtime-cwasm` | `ERYX_RUNTIME_CWASM` | *(embedded)* | Path to a [pre-compiled runtime](./precompile.md) (e.g. with numpy/polars baked in). |
| `--stdlib` | `ERYX_STDLIB` | *(embedded)* | Python stdlib directory; only used with `--runtime-cwasm`. |
| `--site-packages` | `ERYX_SITE_PACKAGES` | *(none)* | The site-packages directory `--runtime-cwasm` was pre-initialized with (`eryx-precompile compile --preinit --site-packages DIR`), mounted at `/site-packages` for Python and `--shell-python`. Needed for any module a package imports after pre-initialization (e.g. pandas' `describe()` imports `numpy.rec`); only used with `--runtime-cwasm`. |
| `--environments` | `ERYX_ENVIRONMENTS` | *(none)* | Directory of [environments](#environments), one per subdirectory, that requests mount by name. |
| `--journal-signing-key` | `ERYX_JOURNAL_SIGNING_KEY` | *(random)* | Hex-encoded 32-byte HMAC key for signing replay journals. Prefer the env var, which keeps the key out of the process list. See [Journal signing](#journal-signing-and-the-trust-boundary). |
| `--journal-ttl-secs` | `ERYX_JOURNAL_TTL_SECS` | `604800` (7 days) | How long a signed journal can be replayed. |
| `--max-execution-timeout-ms` | `ERYX_MAX_EXECUTION_TIMEOUT_MS` | `300000` | Ceiling on `resource_limits.execution_timeout_ms`. |
| `--max-callback-timeout-ms` | `ERYX_MAX_CALLBACK_TIMEOUT_MS` | `300000` | Ceiling on `resource_limits.callback_timeout_ms`. |
| `--max-memory-bytes` | `ERYX_MAX_MEMORY_BYTES` | `536870912` (512 MiB) | Ceiling on `resource_limits.max_memory_bytes`. |
| `--max-fuel` | `ERYX_MAX_FUEL` | *(none)* | Optional ceiling on `resource_limits.max_fuel`; when set, also the budget for requests that set none. Without it fuel is unlimited and the execution timeout is the CPU bound. |
| `--max-callback-invocations` | `ERYX_MAX_CALLBACK_INVOCATIONS` | `10000` | Ceiling on `resource_limits.max_callback_invocations`. |
| `--max-connections` | `ERYX_MAX_CONNECTIONS` | `100` | Ceiling on `network_config.max_connections`. |
| `--max-io-timeout-ms` | `ERYX_MAX_IO_TIMEOUT_MS` | `300000` | Ceiling on `network_config.io_timeout_ms` and `connect_timeout_ms`. |
| `--max-output-bytes` | `ERYX_MAX_OUTPUT_BYTES` | `16777216` (16 MiB) | Most stdout plus stderr bytes kept and streamed per execution, at most 16 MiB. See [Output cap](#output-cap). |
| `--max-open-host-files` | `ERYX_MAX_OPEN_HOST_FILES` | `256` | Most host files (stdlib and site-packages) one execution may hold open; opens past it fail with `EDQUOT`. Not settable per request. |
| `--max-open-host-files-total` | `ERYX_MAX_OPEN_HOST_FILES_TOTAL` | *(half the soft `RLIMIT_NOFILE`, at least 256)* | Most host files all executions in the server may hold open together. |
| `--max-mounts` | `ERYX_MAX_MOUNTS` | `8` | Most [mounts](#mounts) one request may have. |
| `--max-mount-ops` | `ERYX_MAX_MOUNT_OPS` | `1000` | Most `FsRequest`s one execution sends its client; past it, the code's file operations on its mounts fail. |
| `--max-mount-transfer-bytes` | `ERYX_MAX_MOUNT_TRANSFER_BYTES` | `268435456` (256 MiB) | Most bytes of `FsResponse`s a client may send one execution. |
| `--max-mount-cache-bytes` | `ERYX_MAX_MOUNT_CACHE_BYTES` | `16777216` (16 MiB) | Most host memory one execution's cache of mounted file data may hold, evicted oldest first. |
| `--max-mount-listing-bytes` | `ERYX_MAX_MOUNT_LISTING_BYTES` | `16777216` (16 MiB) | Most host memory one execution's directory listings may hold, kept for the execution. |
| `--max-mount-dir-entries` | `ERYX_MAX_MOUNT_DIR_ENTRIES` | `10000` | Most entries one directory's listing may have. |
| `--max-mount-write-bytes` | `ERYX_MAX_MOUNT_WRITE_BYTES` | `67108864` (64 MiB) | Most host memory one execution's changes to its [writable mounts](#writable-mounts) may hold, all together; past it, writes fail with `ENOSPC`. |
| `--allow-plaintext-secrets` | `ERYX_ALLOW_PLAINTEXT_SECRETS` | *(off)* | Let requests set `network_config.allow_plaintext_secrets`, which sends real secret values over unencrypted connections. Off by default: such requests are rejected with `INVALID_ARGUMENT`, so secrets are only substituted over TLS. See [Plain HTTP](./secrets.md#plain-http). |
| `--shell-command` | `ERYX_SHELL_COMMANDS` (`;`-separated) | *(none)* | A shell command, `NAME[,NAME...]=PATH[@sha256:<hex>]`: a `.wasm` or `.cwasm` file, the `.wasm` optionally pinned to its digest. Repeatable; empty entries and an empty variable mean none. See [Commands from a registry](#commands-from-a-registry) and [External Commands](./sandboxes.md#external-commands). |
| `--shell-command-tunnels` | `ERYX_SHELL_COMMAND_TUNNELS` (`,`-separated) | *(none)* | Let a `--shell-command` name open blind `CONNECT` tunnels through the child proxy (no secret substitution, no HTTP rules). A request can turn it off (`commands`), not on. |
| `--shell-command-spawn` | `ERYX_SHELL_COMMAND_SPAWN` (`,`-separated) | *(none)* | Let a `--shell-command` name run commands itself, as git runs `git`. A request can turn it off (`commands`), not on. |
| `--max-child-host-bytes` | `ERYX_MAX_CHILD_HOST_BYTES` | `67108864` (64 MiB) | Ceiling on `resource_limits.max_child_host_bytes`, and the value for requests that set none: the host memory a shell execution's commands' arguments and environments may hold at once. |
| `--max-child-memory-bytes` | `ERYX_MAX_CHILD_MEMORY_BYTES` | `268435456` (256 MiB) | Ceiling on `resource_limits.max_child_memory_bytes`, and the value for requests that set none: the memory a shell execution's running commands may hold together, on top of `--max-memory-bytes`. Raising it raises the host memory one execution can use. |
| `--tls-cert` | `ERYX_TLS_CERT` | *(off)* | PEM certificate chain; enables TLS. Requires `--tls-key`. |
| `--tls-key` | `ERYX_TLS_KEY` | *(off)* | PEM private key for `--tls-cert`. |
| `--tls-client-ca` | `ERYX_TLS_CLIENT_CA` | *(off)* | PEM CA bundle to verify client certs; enables mutual TLS. See [Transport security](#transport-security-tls--mtls). |

### Commands from a registry

eryx-server doesn't fetch commands: it loads `--shell-command` files. To run commands published to an OCI registry (such as `ghcr.io/eryx-org/commands/eryx/rg`), fetch them into a volume first, for example in an init container, and pin each one to its digest:

```yaml
# Kubernetes: fetch the commands into a shared volume before the server starts.
initContainers:
  - name: commands
    image: <an image with wkg or oras>
    command: ["wkg", "oci", "pull", "ghcr.io/eryx-org/commands/eryx/rg:15.1.0-r.2", "-o", "/commands/rg.wasm"]
    volumeMounts: [{ name: commands, mountPath: /commands }]
containers:
  - name: eryx-server
    env:
      - name: ERYX_SHELL_COMMANDS
        value: "rg=/commands/rg.wasm@sha256:<hex>"
    volumeMounts: [{ name: commands, mountPath: /commands, readOnly: true }]
```

- **Fetching:** [Commands from a Registry](./precompile.md#commands-from-a-registry) shows the wkg and oras commands. For a private registry, give the init container credentials the way those tools take them, such as `docker login` or a mounted Docker config. For ghcr.io, that's a token with `read:packages`.
- **The pin:** `@sha256:<hex>` is the digest of the `.wasm` itself. That's the manifest's layer digest, which eryx-commands' `commands.lock` records. It isn't the manifest digest.
- **At startup:** a pinned file that doesn't match fails startup, naming the flag. The command is then checked (a command importing what the shell doesn't provide is refused) and compiled once into eryx's cache directory (`commands/` under the first usable of `$XDG_CACHE_HOME/eryx`, the platform cache directory (`~/.cache/eryx` on Linux) and `$TMPDIR/eryx-<uid>`), so a later start with the same file skips compiling.
- **The cache directory** should be writable and private to the server's user, so pinned commands compile once; mount it as a volume to keep them across restarts. A read-only root filesystem with a writable `/tmp` still caches, in `$TMPDIR/eryx-<uid>`. Only when none of those directories is usable does the server log a warning and compile each pinned command in memory at every start. A wrong pin or a command the shell can't run still stops startup.
- **Pins and `.cwasm`:** only a `.wasm` can be pinned. A `.cwasm` is native code and is trusted as a whole, like `--runtime-cwasm`.

### Shell requests

A request with `language = LANGUAGE_SHELL` runs a shell script against the server's commands: the embedded coreutils, unless `--no-shell-embedded-commands` is set, any `--shell-command`s, `python`/`python3` with `--shell-python` (from the server's Python runtime: `--runtime-cwasm`, `--stdlib` and `--site-packages`, or the embedded one), and `node`/`js` with `--shell-node` (the embedded JavaScript runtime).
- **Which commands:** `allowed_commands` narrows a request to some of them.
- **Per command:** `commands` narrows what each one gets: a narrower network for its children, or no tunnels or spawning. A server older than this field ignores it, as proto3 does any unknown field, so a client can only count on `no_tunnels`, `no_spawn` or a narrower network being enforced by a server new enough to know `commands`.
- **Network:** commands only reach it through `network_config.child_network`. There's none without it.
- **Limits:** `resource_limits.max_child_host_bytes` bounds their arguments and environments, under `--max-child-host-bytes`, and `resource_limits.max_child_memory_bytes` the memory they hold together, under `--max-child-memory-bytes`.
- **Results:** `ExecuteResult.exit_code` is the script's exit status. Trace events report each command as `process_start` and `process_end`, with `pid` and `exit_code`; a command spawned by another command carries its spawner's pid as `ppid`.
- **Sessions:** `persist_state` and `state_snapshot` carry the shell's variables and functions across requests, as for the other guests.

### Environments

`--environments <dir>` (`ERYX_ENVIRONMENTS`) serves the [environments](./environments.md) in `dir`, one per subdirectory, each packed with `eryx-precompile env pack`. A request mounts one read-only by its manifest name, in `ExecuteRequest.environment`, and imports its packages.

```sh
eryx-precompile env pack ./tools --name tools --out envs/tools
eryx-server --environments envs
```

- **At startup:** each environment is loaded and its content hash checked. One that fails to load, one with an empty name, two with the same name, or one for a language the server doesn't serve (not in `--languages`, or built without its cargo feature) stops the server. Symlinks to environments are followed, and entries starting with `.` are skipped, so a mounted volume works.
- **Requests:** an empty `environment` mounts none. A name the server doesn't have, or an environment for another language than the request's, is `INVALID_ARGUMENT`.
- **Pools:** each environment gets its own sandbox pool, sized by the pool flags like a language's and created on its first request. The pool metrics have an `environment` label, empty for the languages' own pools.
- **Replay:** a callback journal is bound to the environment it was recorded under, by name and content hash, so repacking an environment (even under the same name) invalidates the journals recorded with the old one.

### Request limits

Each request sets its own limits in `resource_limits` (and, for networking, `network_config`). A zero field takes the default documented in the proto (30 s execution timeout, 10 s callback timeout, 128 MiB memory, 1000 callback invocations, unlimited fuel). The `--max-*` flags cap what a request can ask for: **a value above its ceiling is clamped down to it, not rejected**, so clients written for a more generous server keep working. Every ceiling must be greater than zero; the server refuses to start otherwise. The memory limit is enforced on the guest, so an allocation past it raises `MemoryError` inside Python.

Fuel is optional. Without `--max-fuel`, requests that set no `max_fuel` run with unlimited fuel, and the execution timeout (capped by `--max-execution-timeout-ms`) is what bounds CPU use. There is no fuel value that suits every workload, so set `--max-fuel` only if you need instruction-level bounds.

### Health checks

The server implements the standard [`grpc.health.v1.Health`](https://github.com/grpc/grpc/blob/master/doc/health-checking.md) service. Both the overall status (service `""`) and `eryx.v1.Eryx` report `SERVING` once the sandbox pool is warm, so Kubernetes' built-in gRPC probes or `grpc_health_probe` work directly:

```yaml
readinessProbe:
  grpc:
    port: 50051
```

During shutdown the listener closes, so new probes fail to connect.

### Shutdown

On Unix, SIGTERM or SIGINT stops the gRPC listener and drains existing RPCs before the process exits. Active Python execution and callback replies can finish over their existing streams. Ctrl-C also initiates shutdown on other platforms.

Set the container or pod termination grace period to cover the request's execution and callback deadlines, any pre-stop delay, and telemetry shutdown. The server does not impose an additional drain timeout. A forced kill or client disconnection can still interrupt execution; draining does not make an uncommitted result recoverable or permit replaying external actions.

## The Execute RPC

The service exposes a single method:

```protobuf
service Eryx {
  rpc Execute(stream ClientMessage) returns (stream ServerMessage);
}
```

It is **bidirectional streaming**. A `ClientMessage` is an `ExecuteRequest`, a `CallbackResponse` or an `FsResponse`; a `ServerMessage` is a `CallbackRequest`, `OutputEvent`, `TraceEvent`, `FsRequest` or `ExecuteResult`. The protocol for one execution is:

1. The client sends an **`ExecuteRequest`** as the first message. This is the only `ExecuteRequest` on the stream — it configures the entire run.
2. As the script runs, the server streams **`OutputEvent`** messages (stdout/stderr chunks) and, when `enable_tracing` is set, **`TraceEvent`** messages.
3. When the Python code `await`s a declared callback, the server sends a **`CallbackRequest`** carrying a unique `request_id`, the callback `name`, and the JSON-encoded `arguments_json`.
4. The client handles the callback and replies with a **`CallbackResponse`** echoing the same `request_id`. Multiple callbacks can be in flight at once (e.g. from `asyncio.gather`), so responses are matched by `request_id`, not by order.
5. When execution finishes, the server sends a single **`ExecuteResult`** and closes the stream.

With [mounts](#mounts), the server also sends **`FsRequest`** messages as the code uses them, and the client answers each with an **`FsResponse`** carrying its `op_id`.

If the client disconnects, the server cancels the in-flight execution, including CPU-bound code, and returns the sandbox to the pool promptly instead of waiting for timeouts. A client may instead half-close its side of the stream (e.g. grpc-go's `CloseSend()`) and keep reading: execution then runs to completion, but any callback the script awaits after that fails immediately, since no response can arrive.

## Configuring a request

`ExecuteRequest` mirrors the sandbox configuration available in the Rust API. Each knob maps to a feature documented elsewhere in this guide:

| Field | Description |
|-------|-------------|
| `code` | The Python source to execute (required). |
| `callbacks` | [Callback](./callbacks.md) declarations (name, description, parameters) that become `async` functions in Python. |
| `resource_limits` | Execution timeout, callback timeout, memory cap, max callback count, [fuel limit](./resource-limits.md), and for the shell `max_child_host_bytes` and `max_child_memory_bytes`. Zero means "use the default". |
| `network_config` | [Networking](./networking.md) policy. **Absent disables networking entirely**; present enables it with the configured host allow/block lists and timeouts. Its `child_network` gives a shell's commands network through a proxy: `MODE_INHERIT` (this policy) or `MODE_NARROW` with `narrow` (only what both allow; `narrow` starts from the defaults, so it must repeat any `allowed_networks`, `internal_hosts` or `blocked_*` overrides children need); absent, they have none. Shell requests only. The request's `max_connections` covers the guest's connections and its children's together. See [External Commands](./sandboxes.md#external-commands). |
| `secrets` | [Secrets](./secrets.md) injected as environment variables with placeholder values; real values are substituted only in HTTP headers to allowed hosts. The shell exports the placeholders to its commands, and only their `child_network` proxy substitutes the real values; commands the server allows tunnels (`--shell-command-tunnels`) send placeholders as they are. With networking on, a secret that has no `allowed_hosts` of its own and no `network_config.allowed_hosts` to fall back on (e.g. under `allow_all_hosts`) is rejected with `INVALID_ARGUMENT`. |
| `disable_stdout_scrub` / `disable_stderr_scrub` | Turn off placeholder scrubbing on stdout/stderr (scrubbing is **on** by default when secrets are present). |
| `files` | [Supporting files](./vfs.md) written into the sandbox VFS — `FILE_KIND_MODULE` files go on `sys.path`, `FILE_KIND_DATA` files are readable but not importable. |
| `mounts` | Directories under `/mnt/` that the client serves as the code uses them, read-only unless `writable`. See [Mounts](#mounts). |
| `persist_state` / `state_snapshot` | [Session state](./sessions.md): when `persist_state` is true the server snapshots state into `ExecuteResult.state_snapshot`; supplying `state_snapshot` restores it before execution. |
| `enable_tracing` | Stream `TraceEvent` messages for line/call/return/exception/callback events, and for the shell's child processes (`process_start`/`process_end`, with `pid`, `exit_code`, and `ppid` for a nested child). `context_json` carries a `callback_start`'s arguments and a `process_start`'s `{"args": [...]}`, with secret placeholders replaced by `[REDACTED]`; context counts against the execution's trace budget. |
| `result_variable` / `scrub_result` | Capture a [result variable](./callbacks.md) (default `result`) into `ExecuteResult.result` as JSON. `scrub_result` opts the result into placeholder scrubbing (off by default — the result is a programmatic side channel). |
| `globals` | Data for the script: a JSON object, as a string, whose top-level keys become globals before the code runs (Python and JavaScript; the shell rejects it). See [Globals](#globals). |
| `env` | [Environment variables](./sandboxes.md#environment-variables) for the code, by name: in `os.environ`, `process.env`, or exported in the shell for its commands. See [Environment variables](#environment-variables). |
| `callback_journal` | Opt into [callback replay](#callback-replay). See below. |
| `language` / `allowed_commands` | The guest language (unset is Python). For the shell, `allowed_commands` narrows this request to some of the server's commands: unset allows all, set with no names allows none, and other commands exit 127. The shell's `grep` builtin and its nested `sh`/`bash` count as commands; other builtins and callbacks aren't restricted. Setting it for another language is `INVALID_ARGUMENT`. |
| `environment` | The name of one of the server's `--environments` to mount (empty: none). It must be for the request's `language`. |
| `commands` | Shell only: per-command options for this request, by the server's command names (`CommandOptions`). `network` gives that command's children `MODE_NONE`, `MODE_INHERIT` (the request's `child_network`) or `MODE_NARROW` (only what both `narrow` and the request's allow, counted against both's limits); `no_tunnels` and `no_spawn` turn off what the server's `--shell-command-tunnels` and `--shell-command-spawn` grant. A command spawned by another gets its own options, not its caller's, so pair a `MODE_NONE` or `MODE_NARROW` network with `no_spawn` to stop it reaching a command with more network. Options can only take away: a name the server doesn't have, network for a command when `child_network` gives children none, or `commands` on another language is `INVALID_ARGUMENT`. |

### Globals

`globals` hands data to a script without editing its code. Send `{"inputs": [1, 2], "args": {"verbose": true}}` and the script reads `inputs` and `args` as ordinary globals (in JavaScript, properties of `globalThis`). The server parses the JSON in the guest, after its own preamble, so the code is passed through untouched: a Python script can still start with `from __future__ import ...`, leading JavaScript `import`s still work, line numbers in errors match the code, a nested `"__proto__"` key stays data, and big integers keep their exact value in Python.

- Each key must match `[A-Za-z_][A-Za-z0-9_]*`, must not be a keyword of the language, and must not be a name the guest already has (a Python builtin or any `__dunder__`, a JavaScript global such as `fetch` or `process`) or the name a callback is installed under. Those, a value that isn't a JSON object, or `globals` on a shell request are `INVALID_ARGUMENT`. Empty means no globals.
- It isn't scrubbed: printing a global prints its value. Put credentials in `secrets`, not here.
- With `persist_state`, globals are saved in the snapshot like any other variable, and the next request's `globals` overwrite those with the same names. In JavaScript that includes a restored `let`; a restored `const` can't be overwritten, so the execution fails with a `TypeError`.
- It counts towards the server's message size limit like `code`.

### Environment variables

`env` gives the code ordinary configuration, such as `{"GIT_AUTHOR_NAME": "Eryx"}`, in every language: Python's `os.environ`, JavaScript's `process.env`, and exported in the shell, so its commands inherit it.

- Each name must match `[A-Za-z_][A-Za-z0-9_]*` and must not be one the sandbox sets itself (`HOME`, `PATH`, `PWD`, `USER`, `TMPDIR`, `TERM`, `LANG`, `LC_ALL`) or a secret's name, and a value must not contain a NUL byte; otherwise the request is `INVALID_ARGUMENT`. Quotes and newlines in a value arrive as sent, never as code.
- It isn't scrubbed: printing a variable prints its value. Put credentials in `secrets`, not here.
- With `persist_state`, a request's variables replace restored values of the same names. The shell's snapshot keeps what an earlier request exported, its `env` included, like any other shell variable; Python and JavaScript start each request with only that request's variables.
- A [callback journal](#callback-replay) is bound to the variables it was recorded with.
- It counts towards the server's message size limit like `code`.

### Mounts

`mounts` gives the code directories the client serves, without uploading them first: `mounts: [{path: "/mnt/repo"}]`. As the code uses one, the server sends `FsRequest`s for it, and the client answers each with an `FsResponse` carrying the request's `op_id`. `mount` is the mount's path, and `path` is relative to it (`""` for the mount itself). The client decides what each path serves, so don't serve files such as `.env` or `.git/config` unless you mean to.

| `FsRequest.op` | Answer with |
|----------------|-------------|
| `stat` | `stat`: an `FsMetadata` (`is_dir`, `size`, `modified` in seconds since the epoch). |
| `list` | `listing`: the directory's entries, each with its metadata. Put a small file's whole contents in its entry's `content`: the server keeps those up to 64 KiB, so reading a tree of small files costs one round trip per directory. |
| `read` | `data`: up to `len` bytes of the file from `offset`, fewer at its end. |

Any of them can instead be answered with `error`: `FS_ERROR_CODE_NOT_FOUND`, `_NOT_A_DIRECTORY`, `_IS_A_DIRECTORY` and `_PERMISSION_DENIED` reach the code as the matching exception (`FileNotFoundError`, ...), anything else as an `OSError` (EIO). A read's error is always EIO, as the file is already open by then: to refuse a file, refuse its `stat`. Once the code has listed a directory, its listing answers stats of its entries, and a name that isn't in it is not found without asking.

- Mounts are read-only unless `writable` (see below): writing, creating or deleting files in them fails.
- A run reads each thing once: a file changed on the client mid-run may be seen as it was, and reads stop at the size the file had when the code first looked at it. Reads fetch 256 KiB at a time.
- Mounts can't overlap, and the [`--max-mount*` flags](#running-the-server) bound each run: how many mounts it may have, how many `FsRequest`s it makes and bytes of `FsResponse`s it's sent (past either, the code's file operations on its mounts fail), and the host memory its file data and listings may hold. Each `FsResponse` must fit in the server's `--max-message-size`.
- They work for every language. They aren't saved with `persist_state`, nor recorded in a [callback journal](#callback-replay): a replayed run asks for them again.
- If the client half-closes or disconnects, file operations waiting for an answer, and any after, fail at once.

### Writable mounts

With `writable: true`, the code can change a mount, but the client's files are never written during the run. The server keeps the changes, over the client's files, and once the code finishes it sends them as `FsChange` messages, just before the `ExecuteResult`, whose `fs_changes` counts them. Apply them in the order they come:

| `FsChange.change` | Meaning |
|-------------------|---------|
| `remove` | Remove the file, or the directory (empty by then: removals come deepest first). |
| `mkdir` | Make the directory (parents come first). |
| `write` | Part of the file's whole new contents: chunks of up to 1 MiB in order, the first at `offset` 0, which creates or truncates the file. An empty file is one empty chunk. |

- Changes come for a failed run too, as far as its code got, so check `ExecuteResult.success` before applying them. None come if the run is cancelled or [suspends](#suspension), since a resumed run starts again from the client's files.
- The changes come from the sandboxed code: treat them as untrusted. Resolve each path inside the mount without following symlinks out of it, and think twice before applying writes to files that run code, such as `.git/hooks`, build scripts or `.env`.
- Writing part of a file fetches it whole first; replacing it, or truncating it to nothing (`open(path, 'w')`), doesn't. The changes may hold up to `--max-mount-write-bytes`, past which writes fail with `ENOSPC`.
- Renaming a file works. Renaming a directory the client served fails with `EXDEV`, which `shutil.move` and `mv` handle by copying; one the code made itself renames.
- Changes are against what the run saw: a file the client changed mid-run isn't noticed. Times aren't reported.

## Reading the result

`ExecuteResult` is the final message. Its key fields:

| Field | Description |
|-------|-------------|
| `success` | `true` only when the script ran to completion without an uncaught exception or sandbox failure. |
| `stdout` / `stderr` | Complete captured output (scrubbed when scrubbing is on). |
| `error` | Human-readable message for a **sandbox** failure. Empty for a script exception and on success — branch on `failure_kind`, not on this string. |
| `failure_kind` | A `FailureKind` enum classifying *why* a run failed (see below). |
| `result` / `result_error` | The captured result variable (JSON), or why capture failed. |
| `stats` | Duration, callback count, peak memory, fuel consumed, and `replayed_callbacks`. Also filled in for failed runs (fuel is 0 for a suspended run). |
| `state_snapshot` | Post-execution state, when `persist_state` was set. |
| `state_skipped_globals` | Names of the globals `state_snapshot` left out because they could not be serialized. |
| `state_was_reset` | Set when restoring `state_snapshot` was interrupted (e.g. by the execution timeout), so the code ran on a fresh interpreter. The reason is in `state_error`. |
| `callback_journal` | The recorded replay journal, when the request opted in. |
| `suspended` / `suspended_callback` | Set when a callback [suspended](#suspension) execution. |
| `output_truncated` | Set when output was dropped: stdout plus stderr went past the [output cap](#output-cap). |

### Output cap

The server keeps and streams at most `--max-output-bytes` (16 MiB by default) of stdout plus stderr per execution. Output past the cap is dropped from both the `OutputEvent` stream and the final `stdout`/`stderr`. The stream that crossed the cap gets a `[output truncated: exceeded N bytes]` marker, and `output_truncated` is set. The script itself keeps running.

The sandbox also limits how much output one execution can stream: 16 MiB, with a small overhead charged per write, so a script making very many tiny writes can reach it before the server's cap. Output past that limit is dropped too and `output_truncated` is set, but without a marker. That limit is why `--max-output-bytes` can't be set above 16 MiB.

### Failure classification

Rather than parsing the `error` string, branch on `failure_kind`:

| `FailureKind` | Meaning |
|---------------|---------|
| `FAILURE_KIND_UNSPECIFIED` | Success — no failure. |
| `FAILURE_KIND_SCRIPT_FAILED` | The script failed: it raised an uncaught exception (`FAILURE_KIND_SCRIPT_EXCEPTION` is a deprecated alias with the same number). The **traceback is in `stderr`** (CPython-style; JavaScript's uncaught error and stack likewise) and `error` is empty. |
| `FAILURE_KIND_TIMEOUT` | Exceeded the configured wall-clock timeout. |
| `FAILURE_KIND_FUEL_EXHAUSTED` | Exceeded the configured fuel (instruction) limit. |
| `FAILURE_KIND_SANDBOX_ERROR` | Infrastructure failure: WASM trap, invalid input (e.g. NUL bytes in the code), session creation failure. Details in `error`. |
| `FAILURE_KIND_CANCELLED` | The client disconnected before completion. |
| `FAILURE_KIND_SUSPENDED` | A callback requested suspension — an expected control outcome, not a failure. See [Suspension](#suspension). |

This is the distinction between a *script* failure (the user's code raised) and a *sandbox* failure (the machinery couldn't run it), so a client can show a traceback to the user versus retrying or alerting.

## Callback replay

The server implements [callback replay](./callback-replay.md) — journaling each callback result during a run and replaying it on a subsequent run, so a re-run after a late failure doesn't re-invoke callbacks that already succeeded. Over gRPC this is controlled entirely through the `callback_journal` field.

### Presence-based opt-in

Journaling is **opt-in by the presence of the `callback_journal` field**, not by a boolean:

- **Field absent** — no journaling, no replay. Execution behaves exactly as if the feature didn't exist (backward-compatible).
- **Field present but empty (`CallbackJournal{}`)** — journaling is on. The server records every callback invocation and returns the recorded journal in `ExecuteResult.callback_journal`. Nothing is replayed (there's nothing to replay yet).
- **Field present with entries** — replay is on. Recorded results are matched against live invocations and served from cache.

So a replay session is: send an empty `CallbackJournal{}` on the first run, persist the journal that comes back, and echo it on the next run.

### The replay loop

On a replay run, callbacks are matched by **name + canonicalized arguments**, consumed FIFO per key. A matched callback returns its cached result instead of dispatching a `CallbackRequest` to the client. `ExecuteStats.replayed_callbacks` reports how many were served from cache.

The first invocation that doesn't match a remaining cached entry — a *miss* — is treated as divergence: that call **and everything after it** run live for the rest of the execution. This divergence guard is what makes replaying a stored journal safe across an edited or non-deterministic script. The matching model, determinism caveats, and concurrency notes are covered in depth in the [Callback Replay & Suspension](./callback-replay.md) guide.

## Suspension

A callback can defer its work instead of returning a value or error. Over gRPC, the client signals this in its `CallbackResponse`:

- Set `outcome = CALLBACK_OUTCOME_SUSPEND`.
- Carry the opaque reason string in the `error` field of the `result` oneof.

When a callback suspends, the server **halts the guest immediately** (no further Python runs, no further callbacks dispatch) and the `ExecuteResult` reflects it:

- `suspended` is `true` and `suspended_callback` carries the callback `name`, `args_json`, and `reason`.
- `failure_kind` is `FAILURE_KIND_SUSPENDED` and `success` is `false`.
- `callback_journal` holds the prefix journal of everything that completed before the suspension.

**Branch on `suspended` first.** Because the guest is halted, `error`/`success` will indicate a non-success — but that is the *expected* consequence of the suspend, not a real failure. To resume once the dependency is ready, send a new `ExecuteRequest` with the **same `code`** and the returned `callback_journal`: the completed prefix replays from cache and the previously-suspended callback re-runs live (it was never journaled), so the script continues past the suspension point.

> Suspension works even without opting into replay (a client can reply `CALLBACK_OUTCOME_SUSPEND` on any run). In that case there's no journal to record the prefix, so `suspended` and the reason are still reported, but there's nothing to resume from.

## Journal signing and the trust boundary

Replayed journal entries are returned to Python **verbatim** — the server does not re-execute the callback to validate them. A journal is therefore a **trusted input**: a crafted journal could inject arbitrary values into a script. Because the server hands a recorded journal back to the client (who echoes it on the next run), it must defend against tampering when that journal round-trips through storage or an intermediary.

The server signs every outgoing journal with **HMAC-SHA256** (`CallbackJournal.signature`). The MAC covers the journal entries, the script `code`, the `state_snapshot` the run started from (when `persist_state` is set), the declared callback names, the `environment`'s name and content hash (when one is set), the request's `globals` and `env`, and an expiry time (`CallbackJournal.expires_at_unix_secs`, `--journal-ttl-secs` after signing). A signature therefore binds a journal to the exact script, starting state, callback set, environment, globals and environment variables that produced it. On an incoming replay request the server:

- **Verifies** the signature against the request's `code`, `state_snapshot`, callback names, `environment`, `globals` and `env` before matching, and checks the journal has not expired.
- **Rejects** an unsigned non-empty, mis-signed, expired, or mismatched journal with `FAILED_PRECONDITION` before executing Python. A rejected replay never becomes a fresh run: completed callbacks may already have produced external effects.
- Accepts an empty, unsigned journal to start a new journaling session. An empty journal that carries a signature must still pass verification.

To run an edited script, explicitly start a new execution without the old journal. To resume a suspended run, send the same `code`, callbacks, `environment`, `globals`, `env` and `state_snapshot` as the run that suspended, and echo the journal back unchanged (including `expires_at_unix_secs`). Journals signed by servers before this binding was added fail verification and must be discarded.

Configure the signing key explicitly so journals are portable:

```bash
# 32 bytes, hex-encoded (64 hex chars)
export ERYX_JOURNAL_SIGNING_KEY=$(openssl rand -hex 32)
eryx-server
```

All replicas must share the same key for journals to verify across instances. **If no key is configured, the server generates a random ephemeral key and logs a warning** — journals then fail verification after a restart or on another replica, and replay is rejected before execution.

Signing provides **provenance and tamper detection**; it does not (and cannot) stop a callback-answering client from choosing arbitrary values live — it always could. See [Security: journals are a trusted input](./callback-replay.md#security-journals-are-a-trusted-input) for the full discussion.

## Transport security (TLS / mTLS)

By default the server listens over plaintext, which is appropriate for a trusted local network (e.g. a sidecar on the same host). To secure the transport:

- **Server TLS** — provide `--tls-cert` and `--tls-key` (a PEM certificate chain and private key). Clients then connect over TLS.
- **Mutual TLS** — additionally provide `--tls-client-ca` (a PEM CA bundle). The server then **requires** every client to present a certificate signed by one of those CAs and rejects connections that don't.

```bash
# server TLS
eryx-server --tls-cert server.pem --tls-key server.key

# mutual TLS
eryx-server --tls-cert server.pem --tls-key server.key --tls-client-ca clients-ca.pem
```

This matters for [secrets](./secrets.md): a `SecretConfig.value` is transmitted in plaintext within the gRPC message, so the connection must be secured (TLS, mTLS, or a trusted local transport) to protect it in transit.

## Observability

- **Metrics** — a Prometheus exporter serves `/metrics` on `--metrics-addr`, exposing pool gauges (in-use/available/total/max), acquisition and creation counters, and execution counters/histograms.
- **Tracing** — the server reads the W3C `traceparent`/`tracestate` headers from incoming gRPC metadata, so its spans become children of the caller's trace. When a callback is dispatched, `CallbackRequest.trace_context` carries a `traceparent` so the client can link its callback-handling spans back to the server-side span.

## Next Steps

- [Callbacks](./callbacks.md) — defining the callbacks declared in `ExecuteRequest`.
- [Callback Replay & Suspension](./callback-replay.md) — the conceptual model behind replay and suspension.
- [Networking](./networking.md), [Secrets](./secrets.md), [Sessions](./sessions.md) — the features behind the corresponding request fields.
- [Output Streaming](./output-streaming.md) — how streamed `OutputEvent` output is produced.
