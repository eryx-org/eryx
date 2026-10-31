# gRPC Server

Eryx can run as a standalone **gRPC server** (`eryx-server`) that executes Python in a sandbox on behalf of remote clients. A pool of warm sandboxes is shared across requests, and a single bidirectional streaming RPC carries everything a request needs: [callbacks](./callbacks.md), real-time [output streaming](./output-streaming.md), execution tracing, [secrets](./secrets.md), [networking](./networking.md), [session state](./sessions.md), the structured result value, and [callback replay & suspension](./callback-replay.md).

It's the backend for hosted, multi-tenant deployments where sandbox execution runs as a remote service. If you only need to embed the sandbox in a single process, use the [Rust API](../api/rust.md) directly; if you want a tool for an AI assistant over stdio, see the [MCP Server](./mcp-server.md). The gRPC server is for remote, multi-tenant, networked execution.

> The protobuf service definition is the source of truth for the API: [`crates/eryx-server/proto/eryx/v1/eryx.proto`](https://github.com/eryx-org/eryx/blob/main/crates/eryx-server/proto/eryx/v1/eryx.proto). The fields and messages described below are documented inline there.

## Running the server

The easiest way to run the server is the published Docker image, [`sd2k/eryx-server`](https://hub.docker.com/r/sd2k/eryx-server):

```bash
docker run -p 50051:50051 sd2k/eryx-server
```

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
| `--tls-cert` | `ERYX_TLS_CERT` | *(off)* | PEM certificate chain; enables TLS. Requires `--tls-key`. |
| `--tls-key` | `ERYX_TLS_KEY` | *(off)* | PEM private key for `--tls-cert`. |
| `--tls-client-ca` | `ERYX_TLS_CLIENT_CA` | *(off)* | PEM CA bundle to verify client certs; enables mutual TLS. See [Transport security](#transport-security-tls--mtls). |

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

It is **bidirectional streaming**. A `ClientMessage` is either an `ExecuteRequest` or a `CallbackResponse`; a `ServerMessage` is a `CallbackRequest`, `OutputEvent`, `TraceEvent`, or `ExecuteResult`. The protocol for one execution is:

1. The client sends an **`ExecuteRequest`** as the first message. This is the only `ExecuteRequest` on the stream — it configures the entire run.
2. As the script runs, the server streams **`OutputEvent`** messages (stdout/stderr chunks) and, when `enable_tracing` is set, **`TraceEvent`** messages.
3. When the Python code `await`s a declared callback, the server sends a **`CallbackRequest`** carrying a unique `request_id`, the callback `name`, and the JSON-encoded `arguments_json`.
4. The client handles the callback and replies with a **`CallbackResponse`** echoing the same `request_id`. Multiple callbacks can be in flight at once (e.g. from `asyncio.gather`), so responses are matched by `request_id`, not by order.
5. When execution finishes, the server sends a single **`ExecuteResult`** and closes the stream.

If the client disconnects, the server cancels the in-flight execution, including CPU-bound code, and returns the sandbox to the pool promptly instead of waiting for timeouts. A client may instead half-close its side of the stream (e.g. grpc-go's `CloseSend()`) and keep reading: execution then runs to completion, but any callback the script awaits after that fails immediately, since no response can arrive.

## Configuring a request

`ExecuteRequest` mirrors the sandbox configuration available in the Rust API. Each knob maps to a feature documented elsewhere in this guide:

| Field | Description |
|-------|-------------|
| `code` | The Python source to execute (required). |
| `callbacks` | [Callback](./callbacks.md) declarations (name, description, parameters) that become `async` functions in Python. |
| `resource_limits` | Execution timeout, callback timeout, memory cap, max callback count, and [fuel limit](./resource-limits.md). Zero means "use the default". |
| `network_config` | [Networking](./networking.md) policy. **Absent disables networking entirely**; present enables it with the configured host allow/block lists and timeouts. |
| `secrets` | [Secrets](./secrets.md) injected as environment variables with placeholder values; real values are substituted only in HTTP headers to allowed hosts. With networking on, a secret that has no `allowed_hosts` of its own and no `network_config.allowed_hosts` to fall back on (e.g. under `allow_all_hosts`) is rejected with `INVALID_ARGUMENT`. |
| `disable_stdout_scrub` / `disable_stderr_scrub` | Turn off placeholder scrubbing on stdout/stderr (scrubbing is **on** by default when secrets are present). |
| `files` | [Supporting files](./vfs.md) written into the sandbox VFS — `FILE_KIND_MODULE` files go on `sys.path`, `FILE_KIND_DATA` files are readable but not importable. |
| `persist_state` / `state_snapshot` | [Session state](./sessions.md): when `persist_state` is true the server snapshots state into `ExecuteResult.state_snapshot`; supplying `state_snapshot` restores it before execution. |
| `enable_tracing` | Stream `TraceEvent` messages for line/call/return/exception/callback events. |
| `result_variable` / `scrub_result` | Capture a [result variable](./callbacks.md) (default `result`) into `ExecuteResult.result` as JSON. `scrub_result` opts the result into placeholder scrubbing (off by default — the result is a programmatic side channel). |
| `callback_journal` | Opt into [callback replay](#callback-replay). See below. |

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
| `FAILURE_KIND_SCRIPT_EXCEPTION` | The script raised an uncaught Python exception. The **traceback is in `stderr`** (CPython-style) and `error` is empty. |
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

The server signs every outgoing journal with **HMAC-SHA256** (`CallbackJournal.signature`). The MAC covers the journal entries, the script `code`, the `state_snapshot` the run started from (when `persist_state` is set), the declared callback names, and an expiry time (`CallbackJournal.expires_at_unix_secs`, `--journal-ttl-secs` after signing). A signature therefore binds a journal to the exact script, starting state and callback set that produced it. On an incoming replay request the server:

- **Verifies** the signature against the request's `code`, `state_snapshot` and callback names before matching, and checks the journal has not expired.
- **Rejects** an unsigned non-empty, mis-signed, expired, or mismatched journal with `FAILED_PRECONDITION` before executing Python. A rejected replay never becomes a fresh run: completed callbacks may already have produced external effects.
- Accepts an empty, unsigned journal to start a new journaling session. An empty journal that carries a signature must still pass verification.

To run an edited script, explicitly start a new execution without the old journal. To resume a suspended run, send the same `code`, callbacks and `state_snapshot` as the run that suspended, and echo the journal back unchanged (including `expires_at_unix_secs`). Journals signed by servers before this binding was added fail verification and must be discarded.

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
