# eryx-run

Run untrusted Python in an [eryx](https://github.com/eryx-org/eryx) sandbox on
[eryx.run](https://eryx.run), with the same API as
[pyeryx](https://pypi.org/project/pyeryx/), which runs sandboxes in your own
process. Moving between the two is an import:

```python
import eryx_run as eryx  # or: import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("print(2 + 2)")
print(result.stdout_text)  # 4
```

eryx-run is pure Python (it depends on `httpx` and `websockets`), so it
installs anywhere, with no WebAssembly runtime of its own.

```sh
pip install eryx-run
export ERYX_API_KEY=eryx_...   # from https://app.eryx.run
```

## Callbacks

Sandboxed code can `await` functions you provide. They run in your process,
sync or async:

```python
import eryx_run as eryx

callbacks = eryx.CallbackRegistry()

@callbacks.callback(description="Look up a user by id")
async def get_user(id: int) -> dict:
    return {"id": id, "name": "Ada"}

sandbox = eryx.Sandbox(callbacks=callbacks)
result = sandbox.execute("""
user = await get_user(id=123)
print(user["name"])
""")
```

Code calls them with keyword arguments. pyeryx's list-of-dicts form works too:
`callbacks=[{"name": "get_user", "fn": get_user}]`.

## Results and errors

`execute` returns an `ExecuteResult` (`stdout`, `stderr`, `stdout_text`,
`duration_ms`, `fuel_consumed`, `result`, ...). Set a variable called `result`
to return a value:

```python
sandbox.execute("result = {'answer': 42}").result  # {'answer': 42}
```

A run that fails raises what pyeryx raises: `PythonExceptionError` (with
`stdout` and `stderr`) when the code raises, `TimeoutError`, or
`ResourceLimitError`. Requests eryx.run refuses raise an `APIError` subclass:
`AuthenticationError`, `RateLimitError` (with `retry_after`),
`NotFoundError`, `ConflictError`, `InvalidRequestError` or
`ServiceUnavailableError`; `APIConnectionError` if eryx.run can't be reached or
the connection drops mid-run. All of them are `EryxError`s.

From asyncio, use `await sandbox.execute_async(code)`.

## Sessions

A `Session` keeps variables, imports and files under `/data` between runs:

```python
session = eryx.Session()
session.execute("x = 21")
session.execute("print(x * 2)").stdout_text  # "42\n"
session.delete()
```

`Session(id=...)` uses an existing session, such as one another process
created (`session.id`).

## What's different from pyeryx

- **Limits:** `ResourceLimits(execution_timeout_ms=..., max_fuel=...)`; `None`
  (the default) means your org's limit. eryx.run sets memory and callback
  limits itself.
- **Hosted-only arguments:** `environment` (an environment you uploaded),
  `connections` (MCP servers), `network_policies`, and `api_key` / `base_url`
  (defaults `$ERYX_API_KEY` and `$ERYX_URL`).
- **Local-only arguments** raise a `ValueError` saying what to use instead:
  `network`, `secrets` and `scrub_*` (use network policies), `mcp` (use
  connections), `volumes`, `on_stdout` / `on_stderr` and `cancel` (not yet),
  and `result_variable` other than `result`. Sessions don't have
  `snapshot_state` / `restore_state`: eryx.run keeps the state.

See the [API reference](https://eryx.run/docs) for the limits and errors.
