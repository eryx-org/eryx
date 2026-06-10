# Callback Replay & Suspension

When an LLM iterates on a Python script that drives expensive [callbacks](./callbacks.md) — tool calls, API requests, database queries — a failure late in the script normally forces a full re-run, re-invoking every callback that already succeeded. **Callback replay** avoids this by *journaling* callback results during a run and *replaying* them on a subsequent run, so only the callbacks that haven't run yet (or whose inputs changed) actually execute.

**Suspension** is the companion feature: a callback can return [`CallbackError::Suspend`] to halt execution ("I can't answer yet — retry later"). Eryx records what was waiting on, stops the guest immediately, and the recorded journal lets you resume from where you left off once the dependency is ready.

> **Availability.** These features are currently exposed in the **Rust library API** only. Python and JavaScript bindings are tracked in [issue #241](https://github.com/eryx-org/eryx/issues/241); gRPC-server documentation is tracked in [issue #242](https://github.com/eryx-org/eryx/issues/242). (The gRPC server already implements both features, including HMAC-signed journals — it just isn't documented in this book yet.)

## How replay works

Rather than checkpointing the Python interpreter (which can't capture mid-execution frames), eryx records each callback invocation and its result. On resubmission the **entire script is re-executed**, but callbacks that match the recorded journal short-circuit to the cached result instead of making a real call. Because callbacks are the expensive part and the Python between them is comparatively free, this is both fast and robust to arbitrary code structure — loops, conditionals, nested functions all work, because the journal operates on the callback *invocations*, not on the code.

Replay is implemented entirely as a callback wrapper: there are no changes to the WASM runtime, the WIT interface, or the Python code.

### Matching model

Callbacks are matched by their **name plus canonicalized arguments**, treated as a FIFO multiset:

- When a journal is loaded, each recorded result is bucketed by its `(name, args)` key in recorded order. Each live invocation pops the next cached result for its key, so repeated identical calls replay in their original order.
- While replay is active, matching is **independent of invocation order** — a concurrently launched batch (`asyncio.gather`) replays correctly no matter which future the scheduler polls first, because a call is matched by *what it is*, not by its position.

### Divergence guard

The first invocation that does **not** match a remaining cached result for its key — a *miss* — is treated as a divergence from the recorded run: replay stops, and that call *and every subsequent call* run live for the rest of the execution. This is the key safety property: it prevents a stale cached result from being replayed across a real divergence (for example, a script edited to write before it reads).

A caller that signs journals and binds the signature to the exact script (as the gRPC server layer does) rejects an edited script's journal *before* matching even runs, restricting replay to re-runs of the same script.

## Recording a journal

Use [`Sandbox::execute_with_journal`] instead of `execute`. It returns a [`ReplayOutcome`] whose `journal` field holds every callback that completed — even if the script itself errored partway through.

```rust,ignore
use eryx::Sandbox;

let sandbox = Sandbox::embedded()
    .with_callback(fetch_user)
    .with_callback(charge_card)
    .build()?;

let outcome = sandbox.execute_with_journal(code).await;

// `journal` is always populated, even on error — persist it to resume later.
let journal = outcome.journal;
println!("recorded {} callbacks", journal.len());
```

[`ReplayOutcome`] carries:

| Field | Meaning |
|-------|---------|
| `result` | The execution result, exactly as `execute` would return it. |
| `journal` | The [`CallbackJournal`] recorded during this run (always present). |
| `replayed_callbacks` | How many callbacks were served from a previous journal (cache hits). |
| `suspended` | `Some(`[`SuspendedCallback`]`)` if a callback requested suspension. |

The [`CallbackJournal`] derives `serde::Serialize`/`Deserialize`, so you can persist it (database, cache, etc.) between runs.

## Replaying a journal

Supply the previously-recorded journal with [`with_replay_journal`] when building the sandbox, then call `execute_with_journal` again with the same code:

```rust,ignore
use eryx::Sandbox;

let sandbox = Sandbox::embedded()
    .with_callback(fetch_user)
    .with_callback(charge_card)
    .with_replay_journal(previous_journal) // results recorded earlier
    .build()?;

let outcome = sandbox.execute_with_journal(code).await;

// Callbacks that matched the journal returned cached results instead of
// running live.
println!("replayed {} callbacks", outcome.replayed_callbacks);
```

`with_replay_journal` only affects `execute_with_journal`; plain [`Sandbox::execute`] ignores it. Each call to `execute_with_journal` uses fresh replay state, so the same sandbox can be executed repeatedly without the journal cursor leaking between runs.

### Concurrent identity

The replay identity is exactly `(callback name, canonical args)`. FIFO ordering is guaranteed for *sequential* identical calls, but it is **not** a stable per-task identity for *concurrent* identical calls — replay can't preserve which `gather` task happened to get which result without an invocation id. If you need a stable assignment, make each call's identity unique by including a **nonce or correlation key in the callback args** so the calls no longer share a key.

## Suspension

A callback can defer its work by returning [`CallbackError::Suspend`] with an opaque reason string:

```rust,ignore
use eryx::{callback, CallbackError};
use serde_json::Value;

/// Requests human approval for an action.
#[callback]
async fn request_approval(action: String) -> Result<Value, CallbackError> {
    match approval_status(&action).await {
        Status::Granted(value) => Ok(value),
        Status::Pending => Err(CallbackError::Suspend(
            format!("awaiting approval for {action}"),
        )),
    }
}
```

When a callback suspends, eryx:

1. Records a [`SuspendedCallback`] (callback name, arguments, reason) — but does **not** journal the call, so it re-runs live on resume.
2. Poisons the WASM fuel to **halt the guest synchronously**, so no further Python runs, no further callbacks dispatch, and no I/O happens after the suspension point.

Two layers guarantee nothing runs after a suspension: a synchronous gate rejects any callback dispatched after the first suspension (covering later `gather` siblings), and the fuel-poison halt traps the guest before it can do anything else.

Because the guest is halted, `outcome.result` will be an `Err` when a suspension occurs — **branch on `suspended` first** and treat that error as the expected consequence of the suspend rather than a failure:

```rust,ignore
let outcome = sandbox.execute_with_journal(code).await;

if let Some(suspended) = &outcome.suspended {
    // Persist outcome.journal, wait for the dependency named by
    // suspended.reason / suspended.name / suspended.args_json, then resume.
    save_for_later(&outcome.journal, suspended);
    return;
}

let result = outcome.result?; // only reached if not suspended
```

### Resuming

To resume, rebuild the sandbox with the journal from the suspended run via `with_replay_journal` and execute the same code again. The recorded prefix replays from cache; the previously-suspended callback re-runs live (it was never journaled) and, assuming its dependency is now ready, returns a real value so the script continues past the suspension point.

## Determinism and limitations

Replay short-circuits *callbacks* — the Python **between** callbacks always re-executes live on every run. Replay therefore reproduces callback *results*, not whole-program state, and it assumes the script is deterministic given the same callback results. Nondeterminism in the script itself — an unseeded `random`, wall-clock time (`time.time()`, `datetime.now()`), or anything else that varies run to run — is recomputed fresh each time, with three consequences:

- **If it feeds callback arguments**, the recomputed args won't match what was journaled, so those calls *miss* — the divergence guard then runs them, and everything after them, live (re-incurring their cost).
- **If it drives control flow**, the replayed run may take a different path than the recorded one, dispatching a different set of callbacks.
- **Non-callback output is not reproduced** — values the script computes itself rather than via a callback are recomputed, so stdout or the [result variable](../guide/callbacks.md) can differ even when every callback replayed.

The divergence guard keeps this **safe**: a recomputed argument that misses falls back to live execution rather than injecting a stale cached result. But replay is only fully *transparent* for scripts whose callback names, arguments, and control flow are deterministic given the same callback results.

To make a nondeterministic input replayable, **route it through a callback** so it lands in the journal — fetch the current time or a random seed via a callback rather than reading it inside the sandbox, and it will replay deterministically like any other recorded result.

## Security: journals are a trusted input

Replayed journal entries are returned to Python **verbatim** — eryx does not re-execute the callback to validate them. A crafted journal can therefore inject arbitrary values into a script's execution. **Treat the journal as a trusted input.**

The core `eryx` crate is agnostic to signing and trusts whatever journal it receives, so only replay journals from a source you control (a previous run of the same sandbox). When journals round-trip through an untrusted boundary — stored externally, or returned to a client and echoed back — verify integrity first. The gRPC server layer provides HMAC-SHA256 signing that binds a journal to the exact script for exactly this purpose.

## See also

- [Callbacks](./callbacks.md) — defining the callbacks that replay records.
- [Rust API Reference](../api/rust.md) — full type documentation for [`ReplayOutcome`], [`CallbackJournal`], and [`SuspendedCallback`].

[`Sandbox::execute`]: https://docs.eryx.run/latest/api/rust/eryx/struct.Sandbox.html#method.execute
[`Sandbox::execute_with_journal`]: https://docs.eryx.run/latest/api/rust/eryx/struct.Sandbox.html#method.execute_with_journal
[`with_replay_journal`]: https://docs.eryx.run/latest/api/rust/eryx/struct.SandboxBuilder.html#method.with_replay_journal
[`ReplayOutcome`]: https://docs.eryx.run/latest/api/rust/eryx/struct.ReplayOutcome.html
[`CallbackJournal`]: https://docs.eryx.run/latest/api/rust/eryx/struct.CallbackJournal.html
[`SuspendedCallback`]: https://docs.eryx.run/latest/api/rust/eryx/struct.SuspendedCallback.html
[`CallbackError::Suspend`]: https://docs.eryx.run/latest/api/rust/eryx/enum.CallbackError.html#variant.Suspend
