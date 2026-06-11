# gRPC Server

Eryx can run as a standalone **gRPC server** (`eryx-server`) that executes Python in a sandbox on behalf of remote clients. A pool of warm sandboxes is shared across requests, and a single bidirectional streaming RPC carries everything a request needs: [callbacks](./callbacks.md), real-time [output streaming](./output-streaming.md), execution tracing, [secrets](./secrets.md), [networking](./networking.md), [session state](./sessions.md), the structured result value, and [callback replay & suspension](./callback-replay.md).

This is the backend behind hosted integrations such as the Grafana Assistant's `execute_python` tool. If you only need to embed the sandbox in a single process, use the [Rust API](../api/rust.md) directly; if you want a tool for an AI assistant over stdio, see the [MCP Server](./mcp-server.md). The gRPC server is for remote, multi-tenant, networked execution.

> The protobuf service definition is the source of truth for the API: [`crates/eryx-server/proto/eryx/v1/eryx.proto`](https://github.com/eryx-org/eryx/blob/main/crates/eryx-server/proto/eryx/v1/eryx.proto). The fields and messages described below are documented inline there.

## Running the server

The server ships as the `eryx-server` binary in the `eryx-server` crate. Build it with the `embedded` and `preinit` features so the runtime and stdlib are baked in:

```bash
cargo build --release -p eryx-server --features eryx/embedded,eryx/preinit
./target/release/eryx-server
```

A `Dockerfile` is provided at `crates/eryx-server/Dockerfile` (its image sets `ENTRYPOINT ["eryx-server"]` and listens on `[::]:50051`).

Every flag also has an environment variable, so the server is easy to configure in a container:

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--listen-addr` | `ERYX_LISTEN_ADDR` | `[::1]:50051` | Address the gRPC service binds to. |
| `--pool-max-size` | `ERYX_POOL_MAX_SIZE` | `10` | Maximum sandboxes in the pool. |
| `--pool-min-idle` | `ERYX_POOL_MIN_IDLE` | `1` | Idle sandboxes kept warm for low-latency acquisition. |
| `--metrics-addr` | `ERYX_METRICS_ADDR` | `0.0.0.0:9090` | Address of the Prometheus `/metrics` endpoint. |
| `--runtime-cwasm` | `ERYX_RUNTIME_CWASM` | *(embedded)* | Path to a [pre-compiled runtime](./precompile.md) (e.g. with numpy/polars baked in). |
| `--stdlib` | `ERYX_STDLIB` | *(embedded)* | Python stdlib directory; only used with `--runtime-cwasm`. |
| `--journal-signing-key` | `ERYX_JOURNAL_SIGNING_KEY` | *(random)* | Hex-encoded 32-byte HMAC key for signing replay journals. See [Journal signing](#journal-signing-and-the-trust-boundary). |
| `--tls-cert` | `ERYX_TLS_CERT` | *(off)* | PEM certificate chain; enables TLS. Requires `--tls-key`. |
| `--tls-key` | `ERYX_TLS_KEY` | *(off)* | PEM private key for `--tls-cert`. |
| `--tls-client-ca` | `ERYX_TLS_CLIENT_CA` | *(off)* | PEM CA bundle to verify client certs; enables mutual TLS. See [Transport security](#transport-security-tls--mtls). |

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

If the client disconnects, the server cancels the in-flight execution and returns the sandbox to the pool promptly instead of waiting for timeouts.

## Configuring a request

`ExecuteRequest` mirrors the sandbox configuration available in the Rust API. Each knob maps to a feature documented elsewhere in this guide:

| Field | Description |
|-------|-------------|
| `code` | The Python source to execute (required). |
| `callbacks` | [Callback](./callbacks.md) declarations (name, description, parameters) that become `async` functions in Python. |
| `resource_limits` | Execution timeout, callback timeout, memory cap, max callback count, and [fuel limit](./resource-limits.md). Zero means "use the default". |
| `network_config` | [Networking](./networking.md) policy. **Absent disables networking entirely**; present enables it with the configured host allow/block lists and timeouts. |
| `secrets` | [Secrets](./secrets.md) injected as environment variables with placeholder values; real values are substituted only in HTTP headers to allowed hosts. |
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
| `stats` | Duration, callback count, peak memory, fuel consumed, and `replayed_callbacks`. |
| `state_snapshot` | Post-execution state, when `persist_state` was set. |
| `callback_journal` | The recorded replay journal, when the request opted in. |
| `suspended` / `suspended_callback` | Set when a callback [suspended](#suspension) execution. |

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

The server signs every outgoing journal with **HMAC-SHA256** (`CallbackJournal.signature`). The MAC covers both the journal entries and the script `code`, so a signature binds a journal to the exact script that produced it. On an incoming replay request the server:

- **Verifies** the signature against the request's `code` before matching.
- **Discards the entries** of an unsigned, mis-signed, or wrong-script journal and falls back to fresh execution. This is a safe fallback, **not an error** — an edited script simply re-runs everything live.

Configure the signing key explicitly so journals are portable:

```bash
# 32 bytes, hex-encoded (64 hex chars)
export ERYX_JOURNAL_SIGNING_KEY=$(openssl rand -hex 32)
eryx-server
```

All replicas must share the same key for journals to verify across instances. **If no key is configured, the server generates a random ephemeral key and logs a warning** — journals then fail verification after a restart or on another replica, and replay silently falls back to fresh execution.

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
