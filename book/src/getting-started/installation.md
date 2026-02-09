# Installation

Eryx provides language bindings for Rust, Python, and JavaScript. Choose your preferred language below.

## Rust

Add Eryx to your `Cargo.toml`:

```toml
[dependencies]
eryx = { version = "0.3", features = ["embedded"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### Setting up the `embedded` feature

The `embedded` feature embeds a pre-compiled Wasm runtime into your binary for zero-config sandbox creation. Because the pre-compiled runtime is platform-specific (~55MB), it is **not** included in the crate and must be generated once for your machine:

```bash
cargo binstall eryx-precompile   # install the pre-compile tool
eryx-precompile setup            # download runtime + pre-compile for your platform
```

After this one-time setup, `cargo build` will find the cached runtime automatically. You can also set `ERYX_RUNTIME_CWASM=/path/to/runtime.cwasm` to use a specific file.

> **Note:** The `eryx-precompile setup` command is not yet implemented — see [#99](https://github.com/eryx-org/eryx/issues/99). For now, the `embedded` feature requires building from the workspace. If you don't need the embedded runtime, omit the feature and provide your own `runtime.wasm` path via `Sandbox::builder()`.

### Feature Flags

| Feature              | Description                                                                         | Trade-offs                               |
|----------------------|-------------------------------------------------------------------------------------|------------------------------------------|
| `embedded`           | Embeds pre-compiled Wasm runtime + Python stdlib into your binary                   | +55MB binary size; requires one-time setup (see above) |
| `macros`             | Enables the `#[callback]` proc macro for simplified callback definitions            | Adds proc-macro compile time             |
| `preinit`            | Pre-initialization support for ~25x faster sandbox creation                         | Adds `eryx-runtime` dep; requires build step |
| `native-extensions`  | Native Python extension support (e.g., numpy) via late-linking                      | Implies `preinit`; experimental          |

**Recommended**: Start with the `embedded` feature for the simplest setup.

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

## JavaScript

Install from npm:

```bash
npm install @bsull/eryx
```

> **Note:** JavaScript bindings require WebAssembly JSPI support. In Node.js, pass `--experimental-wasm-jspi`. In browsers, Chrome 133+ and Edge 133+ are supported.

### Node.js

```bash
node --experimental-wasm-jspi your-script.js
```

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute('print("Hello from Eryx!")');
console.log(result.stdout);
```

### Browser

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute('print("Hello from the browser!")');
console.log(result.stdout);
```

See the [JavaScript API](../api/javascript.md) reference for full documentation.

---

## Next Steps

- [Quick Start](./quick-start.md) - Build your first sandbox
- [Core Concepts](./core-concepts.md) - Understand how Eryx works
