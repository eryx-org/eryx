# Eryx API Usability Design

**Status:** Partially Implemented
**Date:** December 2025

---

## Table of Contents

1. [Current API Analysis](#current-api-analysis)
2. [Target API Vision](#target-api-vision)
3. [Key Questions and Answers](#key-questions-and-answers)
4. [Decisions Made](#decisions-made)
5. [Building Blocks We Have](#building-blocks-we-have)
6. [Building Blocks We Need](#building-blocks-we-need)
7. [API Design Options](#api-design-options)
8. [Implementation Roadmap](#implementation-roadmap)
9. [Python SDK via PyO3](#python-sdk-via-pyo3)

---

## Current API Analysis

### What We Have Today

The current API requires users to understand internals:

```rust
// Current API - lots of ceremony
let numpy_dir = Path::new("/tmp/numpy");
let extensions = load_numpy_extensions(numpy_dir)?;  // User writes this!

let mut builder = Sandbox::builder();
for (name, bytes) in &extensions {
    builder = builder.with_native_extension(name.clone(), bytes.clone());
}

let sandbox = builder
    .with_python_stdlib("/path/to/stdlib")           // Where do I get this?
    .with_site_packages("/path/to/site-packages")    // And this?
    .with_cache_dir("/tmp/eryx-cache")?
    .build()?;
```

### Problems

1. **Discovery burden**: Users must find and download numpy-wasi, python-stdlib, etc.
2. **Path management**: Users must manage temp directories, extraction, paths
3. **Extension loading**: Users write boilerplate to walk directories and load .so files
4. **No version management**: No way to specify `numpy~=2.0`
5. **Embedded runtime + packages unclear**: Can you use `with_embedded_runtime()` AND packages?

---

## Target API Vision

### Dream API (Rust)

```rust
// Dream API - just works
let sandbox = Sandbox::builder()
    .with_pip_package("numpy~=2.0")
    .with_pip_package("pydantic~=2.0")
    .build()
    .await?;

let result = sandbox.execute(r#"
import numpy as np
print(np.linalg.det([[1,2],[3,4]]))
"#).await?;
```

### Dream API (Python via PyO3)

```python
# Python wrapper - even simpler
from eryx import Sandbox

sandbox = Sandbox(packages=["numpy~=2.0", "pydantic~=2.0"])

result = sandbox.execute("""
import numpy as np
print(np.linalg.det([[1,2],[3,4]]))
""")
```

---

## Key Questions and Answers

### Q1: Can embedded runtime work with packages?

**Current state:** Unclear/Not implemented.

**Analysis:**
- The embedded runtime is pre-compiled from `runtime.wasm` at build time
- It does NOT include any native extensions (numpy, etc.)
- It CAN work with pure-Python packages (just mount site-packages)
- It CANNOT work with native extension packages without re-linking

**Options:**

| Option | Works With | Implementation |
|--------|------------|----------------|
| A. Embedded + pure-Python only | pandas (pure), requests, etc. | Just mount site-packages |
| B. Embedded + native extensions | numpy, pydantic-core | Impossible - extensions must be linked |
| C. Pre-built bundles | Common combos (numpy, etc.) | Build multiple embedded runtimes |

**Recommendation:** Option A for pure-Python, but provide easy path to late-linking for native extensions.

### Q2: Can embedded runtime use mmap?

**Current state:** No - embedded bytes are `include_bytes!()` which loads into memory.

**Analysis:**
- Embedded runtime: `include_bytes!()` → always in memory
- Mmap requires a file path → cannot mmap from embedded bytes
- Embedded IS already precompiled, so it's fast (~16ms) even without mmap

**The real question:** Is there a way to get mmap benefits with embedded?

**Options:**

| Option | Speed | Memory | Complexity |
|--------|-------|--------|------------|
| Current embedded | ~16ms | ~85MB RSS per sandbox | Low |
| Write embedded to temp file + mmap | ~3ms | ~8MB RSS per sandbox | Medium |
| Ship as separate file (not embedded) | ~3ms | ~8MB RSS per sandbox | Medium |
| Lazy extraction from embedded | ~3ms after first | ~8MB after first | High |

**Recommendation:** Add `with_embedded_runtime_cached(path)` that:
1. First call: writes embedded bytes to file, mmap loads
2. Subsequent calls: mmap loads from existing file

### Q3: How to prevent misuse with type state?

**Problem:** What if user calls `.with_embedded_runtime().with_native_extension()`? Currently this would silently ignore the native extension.

**Type state pattern:**

```rust
// Type state markers
struct NoRuntime;
struct EmbeddedRuntime;
struct LinkedRuntime;

struct SandboxBuilder<R> {
    runtime: PhantomData<R>,
    // ... other fields
}

impl SandboxBuilder<NoRuntime> {
    pub fn with_embedded_runtime(self) -> SandboxBuilder<EmbeddedRuntime> { ... }
    pub fn with_native_extension(self, ...) -> SandboxBuilder<LinkedRuntime> { ... }
}

impl SandboxBuilder<EmbeddedRuntime> {
    // NO with_native_extension method - compile error!
    pub fn build(self) -> Result<Sandbox, Error> { ... }
}

impl SandboxBuilder<LinkedRuntime> {
    // NO with_embedded_runtime method - compile error!
    pub fn build(self) -> Result<Sandbox, Error> { ... }
}
```

**Downside:** Complex generics, harder to read errors, breaks builder chaining ergonomics.

**Alternative:** Runtime check with clear error message:

```rust
fn build(self) -> Result<Sandbox, Error> {
    if self.has_embedded_runtime && !self.native_extensions.is_empty() {
        return Err(Error::Configuration(
            "Cannot use embedded runtime with native extensions. \
             Remove with_embedded_runtime() or use with_pip_package() instead."
        ));
    }
    // ...
}
```

**Recommendation:** Start with runtime checks, add type state later if needed.

---

## Decisions Made

### D1: Embedded stdlib - DECIDED

**Decision:** Embed stdlib compressed (~2MB zstd), extract to tmpdir on first use.

**Implementation:**
- `python-stdlib.tar.zst` embedded in eryx crate via `include_bytes!()`
- Extracted to `/tmp/eryx-embedded/python-stdlib/` on first sandbox creation
- Validated by checking for `encodings/` directory
- Cached across process restarts (persistent tmpdir)

**Why not download?** Downloading is annoying for users - they want it to "just work".

### D2: Embedded runtime mmap - DECIDED

**Decision:** Always extract embedded runtime to disk and load via mmap.

**Implementation:**
- Embedded `runtime.cwasm` written to `/tmp/eryx-embedded/runtime-{version}.cwasm`
- Loaded via `Component::deserialize_file()` for mmap benefits
- Version in filename handles upgrades automatically
- User doesn't need to care - `with_embedded_runtime()` handles it

**Benefits:**
- 10x memory reduction (85MB → 8MB per sandbox)
- 3x faster loading after first extraction
- Zero user configuration

### D3: Pure Python packages - DECIDED

**Decision:** Users shouldn't have to unzip/mount packages themselves. `with_wheel()` handles it.

**Implementation:**
- `with_wheel("/path/to/package.whl")` extracts and configures everything
- For pure-Python packages: works with embedded runtime
- For native extension packages: requires late-linking (auto-detected)

### D4: No auto-download (for now) - DECIDED

**Decision:** Skip `with_pip_package()` for now. Focus on local wheel support first.

**Rationale:**
- wasi-wheels is very limited (only numpy, a few others)
- Version resolution is complex
- Network dependencies complicate testing
- Can add later when wasi-wheels ecosystem matures

### D5: Python SDK via PyO3 - DEFERRED

**Decision:** Defer for now, but it's a good idea for the future.

**Rationale:**
- Rust API needs to stabilize first
- PyO3 async is complex
- Want to nail the core experience first

---

### Q4: Can we auto-download packages?

**Yes, with caveats:**

```rust
// What with_pip_package() would do:
impl SandboxBuilder {
    pub async fn with_pip_package(self, spec: &str) -> Result<Self, Error> {
        // 1. Parse version spec (e.g., "numpy~=2.0")
        let (name, version) = parse_pip_spec(spec)?;

        // 2. Find WASI-compatible wheel
        let wheel_url = find_wasi_wheel(&name, &version)?;

        // 3. Download (with cache)
        let wheel_bytes = download_with_cache(&wheel_url, &self.cache_dir).await?;

        // 4. Extract wheel
        let extracted = extract_wheel(&wheel_bytes)?;

        // 5. Register native extensions
        for ext in &extracted.native_extensions {
            self = self.with_native_extension(&ext.path, &ext.bytes);
        }

        // 6. Register Python files (mount point)
        self = self.with_site_packages_overlay(&extracted.python_files);

        Ok(self)
    }
}
```

**Challenges:**
1. **WASI wheels are rare** - Only wasi-wheels project provides them
2. **Version resolution** - Need to resolve dependencies
3. **Index format** - wasi-wheels uses GitHub releases, not PyPI
4. **Caching** - Need robust cache for wheels and extracted files

---

## Building Blocks We Have

| Building Block | Status | Notes |
|----------------|--------|-------|
| Late-linking (`link_with_extensions`) | ✅ Complete | Works, ~1.5s for numpy |
| Filesystem cache (`with_cache_dir`) | ✅ Complete | Mmap-based, 3x faster |
| In-memory cache (`with_cache`) | ✅ Complete | For non-filesystem scenarios |
| Pre-initialization | ✅ Complete | Via `eryx::preinit` module |
| Embedded runtime | ✅ Complete | Now with auto mmap via tmpdir |
| Native extension support | ✅ Complete | dlopen table approach |
| Python stdlib mounting | ✅ Complete | WASI preopened dirs |
| Site-packages mounting | ✅ Complete | WASI preopened dirs |
| **Embedded stdlib** | ✅ Complete | 2MB compressed, auto-extracted |
| **Mmap embedded runtime** | ✅ Complete | Auto-extracted to tmpdir |

---

## Building Blocks We Need

### 1. Wheel Extractor (Next Priority)

```rust
pub struct ExtractedWheel {
    /// Python source files (to mount at /site-packages)
    pub python_files: TempDir,
    /// Native extensions (.so files with dlopen paths)
    pub native_extensions: Vec<NativeExtension>,
    /// Package metadata (name, version, dependencies)
    pub metadata: WheelMetadata,
}

pub fn extract_wheel(wheel_path: &Path) -> Result<ExtractedWheel, Error>;
```

### 2. Package Registry Client (Future)

```rust
pub struct WasiWheelRegistry {
    cache_dir: PathBuf,
    index_url: String,  // Default: wasi-wheels GitHub releases
}

impl WasiWheelRegistry {
    /// Find a wheel matching the version spec.
    pub async fn find_wheel(&self, name: &str, version: &str) -> Result<WheelInfo, Error>;

    /// Download a wheel (with caching).
    pub async fn download(&self, wheel: &WheelInfo) -> Result<PathBuf, Error>;
}
```

### 3. Dependency Resolver (Future)

```rust
pub struct DependencyResolver {
    registry: WasiWheelRegistry,
}

impl DependencyResolver {
    /// Resolve all transitive dependencies for a list of packages.
    pub async fn resolve(&self, specs: &[&str]) -> Result<Vec<WheelInfo>, Error>;
}
```

### ~~4. Python Stdlib Bundler~~ ✅ DONE

Implemented via `embedded-stdlib` feature:
- Embedded as `python-stdlib.tar.zst` (~2MB)
- Auto-extracted to `/tmp/eryx-embedded/python-stdlib/`
- Automatically used when no explicit `with_python_stdlib()` call

### ~~5. Mmap-Cached Embedded Runtime~~ ✅ DONE

Implemented - `with_embedded_runtime()` now automatically:
1. Extracts to `/tmp/eryx-embedded/runtime-{version}.cwasm`
2. Loads via `Component::deserialize_file()` for mmap
3. Caches across process restarts

---

## API Design Options

### Option A: Progressive Enhancement

Keep current API, add helpers:

```rust
// Level 1: Current API (full control)
let sandbox = Sandbox::builder()
    .with_native_extension("...", bytes)
    .with_python_stdlib(path)
    .with_site_packages(path)
    .with_cache_dir(path)?
    .build()?;

// Level 2: With wheel helper (less boilerplate)
let sandbox = Sandbox::builder()
    .with_wheel("/path/to/numpy.whl")?
    .build()?;

// Level 3: With auto-download (simplest)
let sandbox = Sandbox::builder()
    .with_pip_package("numpy~=2.0").await?
    .build()?;
```

### Option B: High-Level Wrapper

Separate high-level API:

```rust
// Low-level (current)
use eryx::Sandbox;

// High-level (new)
use eryx::easy::PythonEnvironment;

let env = PythonEnvironment::new()
    .with_package("numpy~=2.0")
    .with_package("pandas~=2.0")
    .build()
    .await?;

let result = env.execute("import numpy; print(numpy.__version__)").await?;
```

### Option C: Configuration File

Support pyproject.toml or eryx.toml:

```toml
# eryx.toml
[sandbox]
python_version = "3.14"
cache_dir = "~/.cache/eryx"

[dependencies]
numpy = "~2.0"
pydantic = "~2.0"
```

```rust
let sandbox = Sandbox::from_config("eryx.toml").await?;
```

### Recommendation

**Option A** - Progressive enhancement is most flexible and backward-compatible.

Add methods in this order:
1. `with_embedded_runtime_cached(path)` - Easy win, big memory improvement
2. `with_wheel(path)` - Local wheel support
3. `with_pip_package(spec)` - Auto-download (biggest UX win)

---

## Implementation Roadmap

### ~~Phase 1: Mmap-Cached Embedded Runtime~~ ✅ DONE

Implemented in `embedded.rs`:
- `EmbeddedResources::get()` extracts runtime to tmpdir
- `with_embedded_runtime()` automatically uses mmap loading
- No user configuration needed

### ~~Phase 1b: Embedded Stdlib~~ ✅ DONE

Implemented in `embedded.rs`:
- `python-stdlib.tar.zst` embedded in crate
- Auto-extracted on first sandbox creation
- Automatically used when `embedded-stdlib` feature enabled

### ~~Phase 2: Package Support~~ ✅ DONE

Implemented via `packages` feature with `with_package()`:

```rust
// Auto-detects format: .whl, .tar.gz, directory
let sandbox = Sandbox::builder()
    .with_package("/path/to/numpy-wasi.tar.gz")?  // tar.gz (wasi-wheels)
    .with_cache_dir("/tmp/cache")?
    .build()?;

// Or with wheel
let sandbox = Sandbox::builder()
    .with_embedded_runtime()
    .with_package("/path/to/requests.whl")?  // standard wheel
    .build()?;
```

Features:
- Auto-detects format from extension
- Extracts to temp directory
- Scans for native extensions (.so) and auto-registers them
- Detects package name from `__init__.py`
- Works with embedded runtime for pure-Python packages

### Phase 3: wasi-wheels Integration (Future)

Deferred - focus on local wheel support first.

### Phase 4: Python SDK (Future)

Deferred - Rust API needs to stabilize first.

---

## Python SDK via PyO3

### Why?

1. **Irony is delicious** - Python SDK for Python sandbox
2. **Familiar API** - Python developers know pip, not cargo
3. **Easy distribution** - `pip install eryx`
4. **Scripting** - Easy to use in notebooks, scripts

### Architecture

```
┌─────────────────────────────────────────────────────┐
│  Python SDK (eryx-py)                               │
│    pip install eryx                                 │
│                                                     │
│    from eryx import Sandbox                         │
│    sandbox = Sandbox(packages=["numpy"])            │
│    result = sandbox.execute("import numpy")         │
└─────────────────────────────────────────────────────┘
                        │
                        │ PyO3 bindings
                        ▼
┌─────────────────────────────────────────────────────┐
│  Rust Core (eryx crate)                             │
│    - Sandbox, SandboxBuilder                        │
│    - PythonExecutor                                 │
│    - Late-linking                                   │
│    - Caching                                        │
└─────────────────────────────────────────────────────┘
                        │
                        │ wasmtime
                        ▼
┌─────────────────────────────────────────────────────┐
│  WASM Sandbox                                       │
│    - Isolated Python execution                      │
│    - numpy, pandas, etc.                           │
└─────────────────────────────────────────────────────┘
```

### PyO3 Bindings

```rust
// New crate: eryx-py/src/lib.rs
use pyo3::prelude::*;

#[pyclass]
struct Sandbox {
    inner: eryx::Sandbox,
    runtime: tokio::runtime::Runtime,
}

#[pymethods]
impl Sandbox {
    #[new]
    #[pyo3(signature = (packages=None, cache_dir=None))]
    fn new(
        packages: Option<Vec<String>>,
        cache_dir: Option<String>,
    ) -> PyResult<Self> {
        let rt = tokio::runtime::Runtime::new()?;

        let inner = rt.block_on(async {
            let mut builder = eryx::Sandbox::builder();

            if let Some(pkgs) = packages {
                for pkg in pkgs {
                    builder = builder.with_pip_package(&pkg).await?;
                }
            }

            builder.build()
        })?;

        Ok(Self { inner, runtime: rt })
    }

    fn execute(&self, code: &str) -> PyResult<String> {
        let result = self.runtime.block_on(self.inner.execute(code))?;
        Ok(result.stdout)
    }
}

#[pymodule]
fn eryx(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<Sandbox>()?;
    Ok(())
}
```

### Python API

```python
from eryx import Sandbox, ExecuteResult

# Simple usage
sandbox = Sandbox(packages=["numpy~=2.0"])
result = sandbox.execute("import numpy; print(numpy.__version__)")
print(result)  # "2.0.0"

# With options
sandbox = Sandbox(
    packages=["numpy~=2.0", "pandas~=2.0"],
    cache_dir="~/.cache/eryx",
    timeout=30.0,
    memory_limit=128 * 1024 * 1024,
)

# Async support (optional, via asyncio)
import asyncio

async def main():
    sandbox = Sandbox(packages=["numpy"])
    result = await sandbox.execute_async("print('hello')")

asyncio.run(main())

# Session support
session = sandbox.session()
session.execute("x = 1")
session.execute("print(x)")  # "1"
```

### Distribution

```toml
# pyproject.toml
[project]
name = "eryx"
version = "0.1.0"
description = "Secure Python sandbox powered by WebAssembly"

[build-system]
requires = ["maturin>=1.0,<2.0"]
build-backend = "maturin"

[tool.maturin]
features = ["pyo3/extension-module"]
```

Build wheels:
```bash
maturin build --release
# Produces: eryx-0.1.0-cp311-cp311-manylinux_2_17_x86_64.whl
```

---

## Open Questions

### 1. Stdlib bundling strategy?

| Option | Crate Size | First Run | Complexity |
|--------|------------|-----------|------------|
| Embed in crate | +50MB | 0ms | Low |
| Download on first use | +0MB | ~5s | Medium |
| Separate stdlib crate | +0MB (main) | 0ms | Medium |

### 2. wasi-wheels version pinning?

wasi-wheels releases are tagged (v0.0.2). Should we:
- Pin to a specific release?
- Always use latest?
- Let user configure?

### 3. Dependency resolution complexity?

Full pip-style resolution is complex. Options:
- No resolution (user specifies exact versions)
- Simple resolution (direct deps only)
- Full resolution (use `resolvo` crate)

### 4. Async in Python SDK?

PyO3 async is tricky. Options:
- Sync-only API (simple)
- `asyncio` integration (complex but nicer)
- Both (most flexible)

---

## Summary

### Current API (with embedded features)

```rust
// Zero-config sandbox! Embedded runtime is automatic when feature is enabled.
let sandbox = Sandbox::builder().build()?;
sandbox.execute("print('Hello!')").await?;

// With packages - late-linking happens automatically for native extensions
let sandbox = Sandbox::builder()
    .with_package("/path/to/numpy-wasi.tar.gz")?  // Has .so files
    .with_cache_dir("/tmp/cache")?                 // For caching linked runtime
    .build()?;

sandbox.execute("import numpy; print(numpy.__version__)").await?;

// Multiple packages work too
let sandbox = Sandbox::builder()
    .with_package("/path/to/numpy-wasi.tar.gz")?
    .with_package("/path/to/scipy-wasi.tar.gz")?  // Hypothetical
    .build()?;
```

### Key Features

- **Automatic runtime selection**: Embedded runtime when no native extensions, late-linking when needed
- **Multiple packages**: Each package mounted at unique path, PYTHONPATH configured automatically
- **Mmap-based loading**: 10x memory reduction, 3x faster loading
- **Zero config**: Just enable features, no method calls needed

### Progress

| Milestone | Status |
|-----------|--------|
| 1. Mmap-cached embedded runtime | ✅ Done |
| 2. Embedded stdlib | ✅ Done |
| 3. Package extraction (`with_package()`) | ✅ Done |
| 4. Multiple packages support | ✅ Done |
| 5. Automatic runtime selection | ✅ Done |
| 6. wasi-wheels integration | 📋 Future |
| 7. Python SDK | 📋 Future |
