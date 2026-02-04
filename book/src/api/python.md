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
