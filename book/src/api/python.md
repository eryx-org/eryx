# Python API Reference

The Python bindings provide a Pythonic interface to Eryx functionality.

For detailed API documentation, see [docs.eryx.run/latest/api/python/](https://docs.eryx.run/latest/api/python/).

## Core Classes

- `Sandbox` - Main class for isolated Python execution
- `Session` - Persistent state across executions
- `SandboxFactory` - Pre-initialize sandboxes with packages
- `SandboxPool` - Bounded pool of warm sandboxes for concurrent execution
- `PooledSandbox` - A sandbox lease from a pool (context manager)
- `PoolStats` - Pool usage statistics
- `VfsStorage` - Virtual filesystem storage
- `ResourceLimits` - Configure execution constraints
- `NetConfig` - Configure network access
- `CallbackRegistry` - Decorator-based callback registration
- `ReplayOutcome` - Result of `Sandbox.execute_with_journal()` (result/error, journal, suspension)
- `SuspendedCallback` - Details of a callback that suspended execution
- `SuspendCallback` - Exception a callback raises to suspend execution

## Installation

```bash
pip install pyeryx
```

Alternatively, see the [PyPI package page](https://pypi.org/project/pyeryx/).

## Returning a structured result

Assign a variable named `result` in the executed script and Eryx JSON-serializes it
and returns it on `ExecuteResult.result` — a structured channel separate from
`stdout`:

```python
import eryx

sandbox = eryx.Sandbox()
out = sandbox.execute('result = {"answer": 42, "items": [1, 2, 3]}')
print(out.result)  # {'answer': 42, 'items': [1, 2, 3]}
```

If the value is not JSON-serializable, `result` is `None` and `result_error`
explains why — execution still succeeds. Pass `result_variable="name"` to
`Sandbox(...)` or `Session(...)` to capture a different variable name.

## Callback replay & suspension

`Sandbox.execute_with_journal(code)` records every callback result in a
journal (a JSON-compatible dict). Pass that journal back as
`Sandbox(replay_journal=...)` and callbacks that match it return the recorded
result instead of running live. A callback can raise
`eryx.SuspendCallback(reason)` to halt the run; resume later by replaying the
journal from the suspended run.

```python
import eryx

def approve(item: str):
    if not is_approved(item):
        raise eryx.SuspendCallback(f"awaiting approval for {item}")
    return True

callbacks = [{"name": "fetch", "fn": fetch}, {"name": "approve", "fn": approve}]
outcome = eryx.Sandbox(callbacks=callbacks).execute_with_journal(code)

if outcome.suspended:
    # Later, once approved: completed callbacks replay, `approve` runs live.
    outcome = eryx.Sandbox(
        callbacks=callbacks, replay_journal=outcome.journal
    ).execute_with_journal(code)
```

`execute_with_journal` does not raise for execution failures; check
`outcome.error`. Journals are a **trusted input** (replayed values reach the
sandbox verbatim), and calls that are not replayed — including the suspended
call on resume — run live again, so side-effecting callbacks must be
idempotent. See the [Callback Replay & Suspension guide](../guide/callback-replay.md).
