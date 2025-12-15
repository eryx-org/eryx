# Native Extensions Support via Late-Linking

## Goal

Enable Python native extensions (numpy, etc.) in eryx sandboxes via **late-linking** at sandbox creation time - NOT bundled into the base component.

## Key Insight

The WASM `dlopen()` is not true dynamic linking - it's a lookup table built at link time. Native extensions must be linked into the component, but we can do this **at sandbox creation** rather than at build time.

```rust
// Fast path - no extensions, use pre-linked component
let sandbox = Sandbox::builder()
    .with_embedded_runtime()
    .build()?;

// Late-linking path - adds numpy at sandbox creation
let sandbox = Sandbox::builder()
    .with_native_extension("numpy", numpy_wasm_bytes)
    .build()?;
```

## numpy WASI Build Available

Pre-built numpy for WASI Python 3.14 from [dicej/wasi-wheels v0.0.2](https://github.com/dicej/wasi-wheels/releases/tag/v0.0.2):

```bash
curl -sL https://github.com/dicej/wasi-wheels/releases/download/v0.0.2/numpy-wasi.tar.gz
```

- 9.6MB compressed, 35MB extracted
- Python 3.14 compatible (`cpython-314-wasm32-wasi`)
- 19 native `.so` modules

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    eryx-runtime (build time)                         │
│                                                                      │
│   Ships base libraries as compressed bytes:                          │
│   ├── libc.so, libc++.so, libc++abi.so                              │
│   ├── libwasi-emulated-*.so                                         │
│   ├── libpython3.14.so                                              │
│   ├── liberyx_runtime.so (our custom runtime)                       │
│   ├── wasi_snapshot_preview1.reactor.wasm                           │
│   └── runtime.wasm (pre-linked for fast path)                       │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    Sandbox::builder().build()                        │
│                                                                      │
│   if native_extensions.is_empty():                                   │
│       Use pre-linked runtime.wasm (fast, ~16ms)                      │
│   else:                                                              │
│       wit_component::Linker to create custom component:              │
│       ├── .library("libc.so", LIBC_BYTES, false)                    │
│       ├── .library("libpython3.14.so", PYTHON_BYTES, false)         │
│       ├── .library("liberyx_runtime.so", RUNTIME_BYTES, false)      │
│       ├── .library("numpy/*.so", user_bytes, true) ← dl_openable    │
│       └── .adapter("wasi_snapshot_preview1", ADAPTER)                │
└─────────────────────────────────────────────────────────────────────┘
```

## Implementation Plan

### Phase 1: Expose Base Libraries (~1 day)

Currently `eryx-runtime/build.rs` links libraries into `runtime.wasm` but doesn't expose them separately. We need to:

1. **Extract individual `.so` files** from the libs directory
2. **Compress and embed them** in eryx-runtime as constants
3. **Export them** so eryx crate can access for late-linking

```rust
// In eryx-runtime/src/lib.rs
pub const LIBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libc.so.zst"));
pub const LIBPYTHON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libpython3.14.so.zst"));
pub const LIBERYX_RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/liberyx_runtime.so.zst"));
// ... etc
```

### Phase 2: Implement Late-Linking (~2 days)

Add `with_native_extension()` to `SandboxBuilder`:

```rust
// In eryx/src/sandbox.rs
impl SandboxBuilder {
    pub fn with_native_extension(
        mut self,
        name: impl Into<String>,
        wasm_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.native_extensions.push((name.into(), wasm_bytes.into()));
        self
    }

    fn link_with_extensions(&self) -> Result<Vec<u8>, Error> {
        use wit_component::Linker;

        let mut linker = Linker::default()
            .validate(true)
            .use_built_in_libdl(true);

        // Add base libraries (from eryx-runtime)
        linker = linker
            .library("libc.so", &decompress(eryx_runtime::LIBC), false)?
            .library("libpython3.14.so", &decompress(eryx_runtime::LIBPYTHON), false)?
            .library("liberyx_runtime.so", &decompress(eryx_runtime::LIBERYX_RUNTIME), false)?
            // ... other base libs
            ;

        // Add user's native extensions (dl_openable = true)
        for (name, bytes) in &self.native_extensions {
            linker = linker.library(name, bytes, true)?;
        }

        linker = linker.adapter("wasi_snapshot_preview1", eryx_runtime::ADAPTER)?;
        linker.encode().map_err(|e| Error::Linking(e.to_string()))
    }
}
```

### Phase 3: Test with numpy (~1 day)

```rust
#[tokio::test]
async fn test_numpy_basic() {
    // Load numpy .so files from wasi-wheels
    let numpy_bytes = std::fs::read("numpy-wasi/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so")?;

    let sandbox = Sandbox::builder()
        .with_native_extension("numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so", numpy_bytes)
        // ... add all 19 numpy modules
        .build()?;

    let result = sandbox.execute(r#"
import numpy as np
a = np.array([1, 2, 3])
print(a.sum())
"#).await?;

    assert!(result.stdout.contains("6"));
}
```

### Phase 4: Helper for numpy Package (~1 day)

Since numpy has 19 .so files plus Python code, add a helper:

```rust
impl SandboxBuilder {
    /// Add numpy support. Loads all numpy native modules from the given directory.
    pub fn with_numpy(mut self, numpy_dir: &Path) -> Result<Self, Error> {
        // Find all .so files in numpy directory
        for entry in walkdir::WalkDir::new(numpy_dir) {
            let entry = entry?;
            if entry.path().extension() == Some("so") {
                let name = entry.path().strip_prefix(numpy_dir)?;
                let bytes = std::fs::read(entry.path())?;
                self = self.with_native_extension(name.to_string_lossy(), bytes);
            }
        }

        // Also need to add numpy Python files to site-packages
        self = self.with_site_packages(numpy_dir.parent().unwrap());
        Ok(self)
    }
}
```

## Challenges

### 1. Python Code Path

The `.so` files are linked into the component, but numpy also has Python code (`.py` files). These need to be accessible via WASI filesystem mount.

**Solution:** Use existing `with_site_packages()` to mount the numpy directory.

### 2. Component Size

Late-linked component with numpy will be larger (~40MB base + 35MB numpy = ~75MB).

**Mitigation:**
- Compress base libraries with zstd
- Cache linked components by extension hash

### 3. Link Time

`wit_component::Linker` takes time to link (~0.5-1s for many libraries).

**Mitigation:**
- Cache linked components
- Only use late-linking when extensions requested

### 4. Pre-initialization

componentize-py does a pre-init step that imports modules and captures memory state for faster cold starts. We skip this for late-linked components.

**Impact:** First `import numpy` is slower (~500ms vs ~50ms).

**Future:** Could implement our own pre-init if this becomes a bottleneck.

## Success Criteria

```python
# This should work in eryx sandbox with late-linked numpy:
import numpy as np

# Basic array operations
a = np.array([[1, 2], [3, 4]])
b = np.array([[5, 6], [7, 8]])
c = np.dot(a, b)
print(c)  # [[19 22] [43 50]]

# Math functions
x = np.linspace(0, 2*np.pi, 100)
y = np.sin(x)
print(f"Max sin: {y.max():.2f}")  # 1.00

# Random numbers
rng = np.random.default_rng(42)
samples = rng.normal(0, 1, 1000)
print(f"Mean: {samples.mean():.3f}")  # ~0.0
```

## Timeline

- Phase 1: Expose base libraries - 1 day
- Phase 2: Late-linking implementation - 2 days
- Phase 3: numpy testing - 1 day
- Phase 4: numpy helper - 1 day

**Total: ~5 days**

## Resources

- [NATIVE_EXTENSIONS.md](../../../plans/NATIVE_EXTENSIONS.md) - Detailed late-linking design
- [native-extensions-research.md](../../../docs/native-extensions-research.md) - Technical deep dive
- [dicej/wasi-wheels](https://github.com/dicej/wasi-wheels) - Pre-built WASI wheels
- [wit-component Linker](https://docs.rs/wit-component/latest/wit_component/struct.Linker.html)
