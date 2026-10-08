# Callback Replay & Suspension

When an LLM iterates on a Python script that drives expensive [callbacks](./callbacks.md) — tool calls, API requests, database queries — a failure late in the script normally forces a full re-run, re-invoking every callback that already succeeded. **Callback replay** avoids this by *journaling* callback results during a run and *replaying* them on a subsequent run, so only the callbacks that haven't run yet (or whose inputs changed) actually execute.

**Suspension** is the companion feature: a callback can return [`CallbackError::Suspend`] to halt execution ("I can't answer yet — retry later"). Eryx records what was waiting on, stops the guest immediately, and the recorded journal lets you resume from where you left off once the dependency is ready.

> **Availability.** This guide covers the **Rust library API**, the **Python bindings** (`Sandbox` only; journals on `SandboxFactory`, `Session` and `SandboxPool`, and per-execute journals, are tracked in [issue #521](https://github.com/eryx-org/eryx/issues/521)) and the **JavaScript bindings** (`@bsull/eryx`, which implement the same matching, divergence guard and suspension semantics in the JS host). The [gRPC server](./grpc-server.md) also implements both features — including HMAC-signed journals — over its `callback_journal` field and `CALLBACK_OUTCOME_SUSPEND` outcome; see the [gRPC Server](./grpc-server.md#callback-replay) guide for the wire-level details.

## How replay works

Rather than checkpointing the Python interpreter (which can't capture mid-execution frames), eryx records each callback invocation and its result. On resubmission the **entire script is re-executed**, but callbacks that match the recorded journal short-circuit to the cached result instead of making a real call. Because callbacks are the expensive part and the Python between them is comparatively free, this is both fast and robust to arbitrary code structure — loops, conditionals, nested functions all work, because the journal operates on the callback *invocations*, not on the code.

Replay is implemented entirely as a callback wrapper: there are no changes to the WASM runtime, the WIT interface, or the Python code.

### Matching model

Callbacks are matched by their **name plus canonicalized arguments**, treated as a FIFO multiset:

- When a journal is loaded, each recorded result is bucketed by its `(name, args)` key in recorded order. Each live invocation pops the next cached result for its key, so repeated identical calls replay in their original order.
- While replay is active, matching is **independent of invocation order** — a concurrently launched batch (`asyncio.gather`) replays correctly no matter which future the scheduler polls first, because a call is matched by *what it is*, not by its position.

### Divergence guard

The first invocation that does **not** match a remaining cached result for its key — a *miss* — is treated as a divergence from the recorded run: replay stops, and that call *and every subsequent call* run live for the rest of the execution. This prevents a stale cached result from being replayed across a real divergence (for example, a script edited to write before it reads). It does **not** prevent re-execution: see [Live re-runs and idempotency](#live-re-runs-and-idempotency).

A caller that signs journals and binds the signature to the exact script (as the [gRPC server layer](./grpc-server.md#journal-signing-and-the-trust-boundary) does) rejects an edited script's journal *before* matching even runs, restricting replay to re-runs of the same script.

## Recording a journal

Use [`Sandbox::execute_with_journal`] instead of `execute` (`executeWithJournal` in JavaScript). It returns a [`ReplayOutcome`] whose `journal` field holds every callback that completed — even if the script itself errored partway through.

<!-- langtabs-start -->

### Rust

```rs
use eryx::Sandbox;

let sandbox = Sandbox::embedded()
    .with_callback(fetch_user)
    .with_callback(charge_card)
    .build()?;

let outcome = sandbox.execute_with_journal(code, None).await;

// `journal` is always populated, even on error — persist it to resume later.
let journal = outcome.journal;
println!("recorded {} callbacks", journal.len());
```

### Python

```python
import json
import eryx

def fetch_user(id: int):
    return {"id": id, "name": "Ada"}

def charge_card(user_id: int, cents: int):
    return {"charged": cents}

code = """
user = await fetch_user(id=1)
await charge_card(user_id=user["id"], cents=500)
"""

sandbox = eryx.Sandbox(callbacks=[
    {"name": "fetch_user", "fn": fetch_user},
    {"name": "charge_card", "fn": charge_card},
])

# Never raises for execution failures: check outcome.error instead.
outcome = sandbox.execute_with_journal(code)

# `journal` is a JSON-compatible dict, always populated, even on error.
saved = json.dumps(outcome.journal)
print(f"recorded {len(outcome.journal['entries'])} callbacks")
```

### JavaScript

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();

// Never rejects for execution failures: check outcome.error instead.
const outcome = await sandbox.executeWithJournal(code);

// `journal` is a JSON string, always populated, even on error. Store it as-is.
await db.save("journal", outcome.journal);
```

<!-- langtabs-end -->

[`ReplayOutcome`] carries:

| Field | Meaning |
|-------|---------|
| `result` | The execution result, exactly as `execute` would return it. |
| `journal` | The [`CallbackJournal`] recorded during this run (always present). |
| `replayed_callbacks` | How many callbacks were served from a previous journal (cache hits). |
| `suspended` | `Some(`[`SuspendedCallback`]`)` if a callback requested suspension. |

The [`CallbackJournal`] derives `serde::Serialize`/`Deserialize`, so you can persist it (database, cache, etc.) between runs.

In Python, `ReplayOutcome` has `result` (an `ExecuteResult`, or `None` on failure), `error` (the exception `execute()` would have raised, or `None`), `journal` (a dict; treat it as opaque), `replayed_callbacks`, and `suspended` (a `SuspendedCallback` or `None`).

In JavaScript, the outcome has the same fields in camelCase: `result` (an `ExecuteResult`, or `undefined`), `error`, `journal`, `replayedCallbacks` and `suspended` (`{ name, argsJson, reason }`). `journal` is a JSON string in the serde format of [`CallbackJournal`], so journals are portable between hosts. Pass it back unmodified: results are stored as their exact JSON text, which a `JSON.parse`/`JSON.stringify` round trip could alter (`1.0` would become `1`, and large integers would lose precision). A JavaScript handler's outcome is journaled as follows:

- A returned JSON string becomes `{"Ok": value}`, byte-for-byte.
- A Python-visible error (throwing a non-`Error` value, such as a string) becomes `{"Err": message}` and replays as the same Python exception.
- A thrown `Error` halts the guest instead of reaching Python. It is recorded as `{"Err": message}` with `"thrown": true`, and replay throws it again, halting at the same point instead of re-running the call. (Remove that entry from the journal to retry the call.)
- Only `SuspendCallback` is not journaled (see [Suspension](#suspension)).

## Replaying a journal

Pass the previously-recorded journal to `execute_with_journal` (`journal=` in Python, `options.journal` in JavaScript) and execute the same code again:

<!-- langtabs-start -->

### Rust

```rs
use eryx::Sandbox;

let sandbox = Sandbox::embedded()
    .with_callback(fetch_user)
    .with_callback(charge_card)
    .build()?;

// `previous_journal` holds the results recorded earlier.
let outcome = sandbox
    .execute_with_journal(code, Some(previous_journal))
    .await;

// Callbacks that matched the journal returned cached results instead of
// running live.
println!("replayed {} callbacks", outcome.replayed_callbacks);
```

### Python

```python
import eryx

def fetch_user(id: int):
    return {"id": id, "name": "Ada"}

callbacks = [{"name": "fetch_user", "fn": fetch_user}]
code = "user = await fetch_user(id=1)"
sandbox = eryx.Sandbox(callbacks=callbacks)
previous = sandbox.execute_with_journal(code).journal

outcome = sandbox.execute_with_journal(code, journal=previous)  # results recorded earlier

# Callbacks that matched the journal returned cached results instead of
# running live.
print(f"replayed {outcome.replayed_callbacks} callbacks")
```

### JavaScript

```javascript
const outcome = await sandbox.executeWithJournal(code, {
  journal: await db.load("journal"), // the string recorded earlier
});

// Callbacks that matched the journal returned cached results instead of
// running live.
console.log(`replayed ${outcome.replayedCallbacks} callbacks`);
```

<!-- langtabs-end -->

Plain [`Sandbox::execute`] never journals or replays (likewise JavaScript's `execute`). Each call to `execute_with_journal` uses fresh replay state, so one sandbox can record, replay and resume different journals without being rebuilt. Sessions (`InProcessSession` in Rust, `Session` in Python) and pooled sandboxes have the same `execute_with_journal` method; in a session, Python state persists across journaled calls as usual.

### Concurrent identity

The replay identity is exactly `(callback name, canonical args)`. FIFO ordering is guaranteed for *sequential* identical calls, but it is **not** a stable per-task identity for *concurrent* identical calls — replay can't preserve which `gather` task happened to get which result without an invocation id. If you need a stable assignment, make each call's identity unique by including a **nonce or correlation key in the callback args** so the calls no longer share a key.

## Suspension

A callback can defer its work by returning [`CallbackError::Suspend`] (raising `eryx.SuspendCallback` in Python, from a sync or async callback; throwing `SuspendCallback` in JavaScript) with an opaque reason string:

<!-- langtabs-start -->

### Rust

```rs
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

### Python

```python
import eryx

APPROVED: set[str] = set()  # stand-in for your approval store

def request_approval(action: str):
    """Requests human approval for an action."""
    if action not in APPROVED:
        raise eryx.SuspendCallback(f"awaiting approval for {action}")
    return {"approved": action}
```

### JavaScript

```javascript
import { SuspendCallback, setCallbackHandler } from "@bsull/eryx";

const approved = new Set(); // stand-in for your approval store

setCallbackHandler((name, argsJson) => {
  if (name === "request_approval") {
    const { action } = JSON.parse(argsJson);
    if (!approved.has(action)) {
      throw new SuspendCallback(`awaiting approval for ${action}`);
    }
    return JSON.stringify({ approved: action });
  }
  // ... other callbacks ...
});
```

<!-- langtabs-end -->

When a callback suspends, eryx:

1. Records a [`SuspendedCallback`] (callback name, arguments, reason) — but does **not** journal the call, so it re-runs live on resume.
2. **Halts the guest synchronously**, so no further Python runs, no further callbacks dispatch, and no I/O happens after the suspension point. The Rust host poisons the WASM fuel; in JavaScript the thrown `SuspendCallback` propagates out of the host import, which rejects the execution immediately (Python cannot catch it).

Two layers guarantee nothing runs after a suspension: a synchronous gate rejects any callback dispatched after the first suspension (covering later `gather` siblings), and the halt stops the guest before it can do anything else.

Because the guest is halted, `outcome.result` will be an `Err` (in Python, `result` is `None` and `error` is an `ExecutionError`; in JavaScript, `result` is `undefined` and `error` is the `SuspendCallback`) when a suspension occurs — **branch on `suspended` first** and treat that error as the expected consequence of the suspend rather than a failure:

<!-- langtabs-start -->

### Rust

```rs
let outcome = sandbox.execute_with_journal(code, None).await;

if let Some(suspended) = &outcome.suspended {
    // Persist outcome.journal, wait for the dependency named by
    // suspended.reason / suspended.name / suspended.args_json, then resume.
    save_for_later(&outcome.journal, suspended);
    return;
}

let result = outcome.result?; // only reached if not suspended
```

### Python

```python
import eryx

def request_approval(action: str):
    raise eryx.SuspendCallback(f"awaiting approval for {action}")

sandbox = eryx.Sandbox(callbacks=[{"name": "request_approval", "fn": request_approval}])
outcome = sandbox.execute_with_journal('await request_approval(action="deploy")')

if outcome.suspended:
    # Persist outcome.journal, wait for the dependency named by
    # suspended.reason / suspended.name / suspended.args_json, then resume.
    print("suspended:", outcome.suspended.reason)
elif outcome.error:
    raise outcome.error
else:
    print(outcome.result.stdout_text)
```

### JavaScript

```javascript
const outcome = await sandbox.executeWithJournal(code);

if (outcome.suspended) {
  // Persist outcome.journal, wait for the dependency named by
  // suspended.reason / suspended.name / suspended.argsJson, then resume.
  console.log("suspended:", outcome.suspended.reason);
} else if (outcome.error) {
  throw outcome.error;
} else {
  console.log(outcome.result.stdout);
}
```

<!-- langtabs-end -->

### Resuming

To resume, execute the same code again with the journal from the suspended run (`execute_with_journal(code, Some(outcome.journal))` in Rust, `execute_with_journal(code, journal=outcome.journal)` in Python, `executeWithJournal(code, { journal: outcome.journal })` in JavaScript). The recorded prefix replays from cache; the previously-suspended callback re-runs live (it was never journaled) and, assuming its dependency is now ready, returns a real value so the script continues past the suspension point.

### Live re-runs and idempotency

Replay never replays a stale result, but it also does not guarantee that a callback runs **at most once**. A call runs live again whenever it is not served from the journal:

- calls that *miss* — the first divergent call and every call after it;
- the previously-suspended call, on resume (it was never journaled);
- calls that never completed into the journal: those cut off by the host's callback timeout, those that never reached the handler, and any call started after the suspending call (the journal is truncated at the suspension point) — for example a `gather` sibling of the suspending call. A call still in flight when the run fails, times out or suspends is otherwise awaited by the host and journaled, as is one the script stopped waiting for (`wait_for`, `task.cancel()`).

Callbacks with side effects (charging a card, sending a message, writing a record) must therefore be **idempotent**, or deduplicate on their side — for example with an idempotency key passed in the callback args. Because a suspending callback is invoked again on resume, it should only check readiness before suspending, and perform its side effects only on the call that returns a value.

Every call that *completed* is journaled, including failures: an error result replays as the same error instead of re-running the callback. In JavaScript this includes a handler that threw an `Error` (which halts the run): replay halts at the same point without calling the handler again.

## Determinism and limitations

Replay short-circuits *callbacks* — the Python **between** callbacks always re-executes live on every run. Replay therefore reproduces callback *results*, not whole-program state, and it assumes the script is deterministic given the same callback results. Nondeterminism in the script itself — an unseeded `random`, wall-clock time (`time.time()`, `datetime.now()`), or anything else that varies run to run — is recomputed fresh each time, with these consequences:

- **If it feeds callback arguments**, the recomputed args won't match what was journaled, so those calls *miss* — the divergence guard then runs them, and everything after them, live (re-incurring their cost).
- **If it drives control flow**, the replayed run may take a different path than the recorded one, dispatching a different set of callbacks.
- **Timers are not journaled** — `asyncio.sleep`, `wait_for` and `asyncio.timeout` deadlines are not callbacks, so a replayed run waits through its sleeps again in real time, while replayed callbacks return instantly. A timeout (or `task.cancel()`) only stops Python waiting: the host callback still completes and is journaled. On replay it returns instantly from cache, so a `wait_for` that timed out on the recording run can now succeed and take a different path. Only a callback that hit the host's callback timeout, or never reached the handler, is missing from the journal.
- **Non-callback output is not reproduced** — values the script computes itself rather than via a callback are recomputed, so stdout or the [result variable](../guide/callbacks.md) can differ even when every callback replayed.

The divergence guard ensures a recomputed argument that misses falls back to live execution rather than injecting a stale cached result — which means that call, and everything after it, runs live again (see [Live re-runs and idempotency](#live-re-runs-and-idempotency)). But replay is only fully *transparent* for scripts whose callback names, arguments, and control flow are deterministic given the same callback results.

To make a nondeterministic input replayable, **route it through a callback** so it lands in the journal — fetch the current time or a random seed via a callback rather than reading it inside the sandbox, and it will replay deterministically like any other recorded result.

> A built-in deterministic mode (a seedable RNG and a mockable clock, captured in the journal) is being explored in [issue #244](https://github.com/eryx-org/eryx/issues/244) to cover these cases without routing each input through a callback.

## Security: journals are a trusted input

Replayed journal entries are returned to Python **verbatim** — eryx does not re-execute the callback to validate them. A crafted journal can therefore inject arbitrary values into a script's execution. **Treat the journal as a trusted input.**

The core `eryx` crate is agnostic to signing and trusts whatever journal it receives, so only replay journals from a source you control (a previous run of the same sandbox). When journals round-trip through an untrusted boundary — stored externally, or returned to a client and echoed back — verify integrity first. The [gRPC server layer](./grpc-server.md#journal-signing-and-the-trust-boundary) provides HMAC-SHA256 signing that binds a journal to the exact script for exactly this purpose.

## See also

- [Callbacks](./callbacks.md) — defining the callbacks that replay records.
- [Rust API Reference](../api/rust.md) — full type documentation for [`ReplayOutcome`], [`CallbackJournal`], and [`SuspendedCallback`].
- [Python API Reference](../api/python.md#callback-replay--suspension) — `Sandbox.execute_with_journal`, `ReplayOutcome`, `SuspendedCallback`, `SuspendCallback`.
- [JavaScript API Reference](../api/javascript.md) — `executeWithJournal`, `ReplayOutcome`, `SuspendCallback`.

[`Sandbox::execute`]: https://docs.eryx.run/latest/api/rust/eryx/struct.Sandbox.html#method.execute
[`Sandbox::execute_with_journal`]: https://docs.eryx.run/latest/api/rust/eryx/struct.Sandbox.html#method.execute_with_journal
[`ReplayOutcome`]: https://docs.eryx.run/latest/api/rust/eryx/struct.ReplayOutcome.html
[`CallbackJournal`]: https://docs.eryx.run/latest/api/rust/eryx/struct.CallbackJournal.html
[`SuspendedCallback`]: https://docs.eryx.run/latest/api/rust/eryx/struct.SuspendedCallback.html
[`CallbackError::Suspend`]: https://docs.eryx.run/latest/api/rust/eryx/enum.CallbackError.html#variant.Suspend
