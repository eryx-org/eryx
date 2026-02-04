# Installation

## Adding Eryx to Your Project

<!-- langtabs-start -->
```toml
// Add to your Cargo.toml:
[dependencies]
eryx = { version = "0.3", features = ["embedded"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```bash
# Install via pip:
pip install pyeryx
```
<!-- langtabs-end -->

> **Python Note:** The package is installed as `pyeryx` but imported as `eryx`.

### Feature Flags (Rust Only)

| Feature              | Description                                                                         | Trade-offs                               |
|----------------------|-------------------------------------------------------------------------------------|------------------------------------------|
| `embedded`           | Zero-config sandboxes: embeds pre-compiled Wasm runtime + Python stdlib             | +32MB binary size; enables `unsafe` code paths |
| `preinit`            | Pre-initialization support for ~25x faster sandbox creation                         | Adds `eryx-runtime` dep; requires build step |
| `native-extensions`  | Native Python extension support (e.g., numpy) via late-linking                      | Implies `preinit`; experimental          |

**Recommended**: Start with the `embedded` feature for zero-configuration setup.

Package support (`with_package()` for `.whl` and `.tar.gz` files) is always available — no feature flag required.

## Verify Installation

<!-- langtabs-start -->
```rust
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let result = sandbox.execute("print('Hello from Eryx!')").await?;
    println!("{}", result.stdout);
    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("print('Hello from Eryx!')")
print(result.stdout)
```
<!-- langtabs-end -->

## Next Steps

- [Quick Start](./quick-start.md) - Build your first sandbox
- [Core Concepts](./core-concepts.md) - Understand how Eryx works
