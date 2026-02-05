# Installation

Eryx provides language bindings for both Rust and Python. Choose your preferred language below.

## Rust

Add Eryx to your `Cargo.toml`:

```toml
[dependencies]
eryx = { version = "0.3", features = ["embedded"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### Feature Flags

| Feature              | Description                                                                         | Trade-offs                               |
|----------------------|-------------------------------------------------------------------------------------|------------------------------------------|
| `embedded`           | Zero-config sandboxes: embeds pre-compiled Wasm runtime + Python stdlib             | +32MB binary size; enables `unsafe` code paths |
| `macros`             | Enables the `#[callback]` proc macro for simplified callback definitions            | Adds proc-macro compile time             |
| `preinit`            | Pre-initialization support for ~25x faster sandbox creation                         | Adds `eryx-runtime` dep; requires build step |
| `native-extensions`  | Native Python extension support (e.g., numpy) via late-linking                      | Implies `preinit`; experimental          |

**Recommended**: Start with the `embedded` feature for zero-configuration setup.

Package support (`with_package()` for `.whl` and `.tar.gz` files) is always available — no feature flag required.

---

## Python

Install from PyPI:

```bash
pip install pyeryx
```

> **Note:** The package is installed as `pyeryx` but imported as `eryx`.

### Verify Installation

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("print('Hello from Eryx!')")
print(result.stdout)
```

---

## Next Steps

- [Quick Start](./quick-start.md) - Build your first sandbox
- [Core Concepts](./core-concepts.md) - Understand how Eryx works
