# Python API Reference

The Python bindings provide a Pythonic interface to Eryx functionality.

For detailed API documentation, see [docs.eryx.run/latest/api/python/](https://docs.eryx.run/latest/api/python/).

## Core Classes

- `Sandbox` - Main class for isolated Python execution
- `Session` - Persistent state across executions
- `SandboxFactory` - Pre-initialize sandboxes with packages
- `VfsStorage` - Virtual filesystem storage
- `ResourceLimits` - Configure execution constraints
- `NetConfig` - Configure network access
- `CallbackRegistry` - Decorator-based callback registration

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
