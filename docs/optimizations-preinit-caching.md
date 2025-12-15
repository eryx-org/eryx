# Performance Optimizations: Pre-initialization & Caching

**Goal:** Reduce sandbox creation time from ~1500-2000ms (with numpy) to <100ms while maintaining late-linking flexibility.

**Status:** Pre-compilation caching implemented ✅, pre-initialization planned

---

## Overview: Three Separate Optimizations

This document covers three **distinct** optimizations that work together:

### 1. Pre-Compilation (We Already Have This ✅)
- **What:** Serialize wasmtime's JIT-compiled native code to `.cwasm` format
- **Saves:** JIT compilation time (500ms → 10ms)
- **Status:** ✅ Working today via `embedded-runtime` feature
- **Implementation:** `PythonExecutor::precompile()` uses `wasmtime::Module::serialize()`

### 2. Pre-Compilation Caching ✅ IMPLEMENTED
- **What:** Cache pre-compiled components by extension hash
- **Saves:** Linking + compilation on repeated builds (1.07s → 61ms)
- **Status:** ✅ Implemented in `crates/eryx/src/cache.rs`
- **Speedup:** ~18x (benchmark measured)
- **Implementation:**
  - `ComponentCache` trait with `FilesystemCache` and `InMemoryCache` implementations
  - `CacheKey` computed from SHA256 of extension bytes + eryx/wasmtime versions
  - `SandboxBuilder::with_cache()` and `with_cache_dir()` methods
  - Benchmark: `cargo bench -p eryx --features native-extensions,precompiled -- caching`

### 3. Pre-Initialization (Need to Implement)
- **What:** Run Python init + imports, capture memory state into component
- **Saves:** Python initialization + first imports (50-100ms → <1ms per execution)
- **Status:** ❌ Not implemented (but `preinit.rs` exists in `feat/late-linking-exploration`)
- **Complexity:** Medium-High (3-4 days)

**Key distinction:** Pre-compilation saves JIT time. Pre-initialization saves Python startup time. They're complementary.

### Visual Comparison

```
┌─────────────────────────────────────────────────────────────────┐
│ Pre-Compilation Caching (Level 2 Caching)                        │
├─────────────────────────────────────────────────────────────────┤
│ First build:                                                     │
│   Link (.wasm) → Compile (JIT) → Serialize (.cwasm) → Cache     │
│   1000ms         500ms           10ms           10ms = 1520ms    │
│                                                                  │
│ Subsequent builds (cache hit):                                  │
│   Load (.cwasm) → Deserialize                                   │
│   1ms            10ms = 11ms                                     │
│                                                                  │
│ Per execution:                                                   │
│   Instantiate + Python init + Execute                           │
│   1ms           50-100ms         <1ms = 51-101ms                 │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Pre-Initialization (component-init-transform)                   │
├─────────────────────────────────────────────────────────────────┤
│ First build:                                                     │
│   Link → Compile → Pre-init (run Python + capture mem) → Cache  │
│   1000ms  500ms    2000ms                              = 3500ms  │
│                                                                  │
│ Subsequent builds (cache hit):                                  │
│   Same as pre-compilation caching = 11ms                        │
│                                                                  │
│ Per execution:                                                   │
│   Instantiate + Execute (Python already initialized!)           │
│   1ms           <1ms = ~1.5ms                                    │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Both Combined (componentize-py's approach)                      │
├─────────────────────────────────────────────────────────────────┤
│ First build: ~3500ms (one-time cost)                            │
│ Subsequent builds: ~11ms (cache hit)                            │
│ Per execution: ~1.5ms (main branch actual: 1.57ms)              │
└─────────────────────────────────────────────────────────────────┘
```

## Table of Contents

1. [Current Performance](#current-performance)
2. [Optimization 1: Component Caching](#optimization-1-component-caching)
3. [Optimization 2: Pre-Initialization](#optimization-2-pre-initialization)
4. [Combined Approach](#combined-approach)
5. [Implementation Plan](#implementation-plan)
6. [Existing preinit.rs Implementation](#existing-preinitrss-implementation)

---

## Current Performance

### Sandbox Creation Breakdown

```
Base runtime (no extensions):
  • Load runtime.wasm file:           ~10ms
  • wasmtime compile to native:       ~500ms
  • WASI context setup:               ~5ms
  • Total: ~515ms

With numpy (late-linking):
  • Decompress base libraries:        ~200ms
  • wit_component::Linker:            ~1000ms
  • wasmtime compile to native:       ~500ms
  • WASI context setup:               ~5ms
  • Total: ~1705ms

Embedded runtime (no extensions):
  • Load pre-compiled bytes:          ~1ms
  • Deserialize (unsafe):             ~15ms
  • WASI context setup:               ~5ms
  • Total: ~21ms
```

### The Problem

**500ms baseline is too slow** for interactive use cases. This is mostly:
- wasmtime JIT compilation (~500ms)
- Python interpreter initialization (~50-100ms on first import)

componentize-py achieves ~50ms by:
1. Pre-compiling to native code (like our `embedded-runtime`)
2. Pre-initializing Python (importing modules, capturing memory)

We can do the same with late-linked components.

---

## Optimization 1: Component Caching

### Goal

Amortize linking cost by caching late-linked components.

### Current Problem

```rust
// Every sandbox creation with numpy does:
let sandbox = Sandbox::builder()
    .with_native_extension("numpy/core/*.so", bytes)  // Same bytes
    .build()?;  // ← Re-links EVERY TIME (~1.7s)
```

### Solution: Hash-Based Caching

```rust
pub struct ComponentCache {
    cache_dir: PathBuf,
}

impl ComponentCache {
    pub fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let hex = hex::encode(key);
        let path = self.cache_dir.join(format!("{hex}.cwasm"));
        std::fs::read(&path).ok()
    }

    pub fn put(&self, key: [u8; 32], component: Vec<u8>) {
        let hex = hex::encode(key);
        let path = self.cache_dir.join(format!("{hex}.cwasm"));
        let _ = std::fs::write(&path, &component);
    }
}

// Compute cache key from extensions
fn compute_cache_key(extensions: &[NativeExtension]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    // Sort by name for determinism
    let mut sorted: Vec<_> = extensions.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for ext in sorted {
        hasher.update(ext.name.as_bytes());
        hasher.update(&(ext.bytes.len() as u64).to_le_bytes());
        hasher.update(&ext.bytes);
    }

    hasher.finalize().into()
}
```

### Two Levels of Caching

**Level 1: Cache linked component bytes** (.wasm format)
```rust
// First call: ~1700ms (link + compile + cache bytes)
// Second call: ~500ms (load bytes + compile)
```
Saves linking time (~1000ms) but still needs wasmtime compilation (~500ms).

**Level 2: Cache pre-compiled component** (.cwasm format - wasmtime serialization)
```rust
// First call: ~1700ms (link + compile + serialize + cache)
// Second call: ~10-16ms (load + deserialize)
```
Saves both linking AND compilation. This is what gives the big speedup.

### Usage (Level 2 - Recommended)

```rust
// First call: ~1700ms (link + compile + pre-compile + cache)
let sandbox1 = Sandbox::builder()
    .with_native_extension("numpy/core/*.so", bytes)
    .with_cache_dir("/tmp/eryx-cache")
    .with_precompiled_cache(true)  // ← Cache pre-compiled, not just linked
    .build()?;

// Second call: ~10-16ms (cache hit + deserialize pre-compiled)
let sandbox2 = Sandbox::builder()
    .with_native_extension("numpy/core/*.so", bytes)
    .with_cache_dir("/tmp/eryx-cache")
    .with_precompiled_cache(true)
    .build()?;
```

**Actual measurements (criterion benchmarks, release mode):**
- Cold (no cache): **1.07s** (link + compile each time)
- Warm (filesystem cache): **61ms** (load from cache + deserialize)
- Warm (memory cache): **62ms** (load from cache + deserialize)
- Speedup: **~18x** on cache hit

Run benchmarks with: `cargo bench -p eryx --features native-extensions,precompiled -- caching`

### Cache Invalidation

Cache should be invalidated when:
- Base library versions change (eryx-runtime version)
- Extension bytes change (different numpy version)
- wasmtime version changes (incompatible pre-compilation)

```rust
pub struct CacheKey {
    eryx_runtime_version: &'static str,   // From env!("CARGO_PKG_VERSION")
    wasmtime_version: &'static str,        // From wasmtime crate version
    extensions_hash: [u8; 32],             // From extension contents
}
```

### Disk Space

- Each cached component: ~57MB (with numpy)
- Max cache size: Configurable (e.g., 10 components = 570MB)
- LRU eviction: Remove least recently used when full

### Implementation Steps

1. Add `ComponentCache` struct with filesystem backing
2. Update `linker::link_with_extensions()` to check cache
3. Add `with_cache_dir()` to `SandboxBuilder`
4. Pre-compile cached components (use wasmtime's pre-compilation)
5. Add cache size limits and LRU eviction

**Estimated time:** 1-2 days

**Performance gain:** 1700ms → 16ms (100x faster on cache hit)

---

## Optimization 2: Pre-Initialization

### Goal

Reduce Python initialization time from ~50-100ms to <1ms by capturing initialized memory state.

### How componentize-py Does It

```
1. Build component with libraries
2. Instantiate in wasmtime
3. Run Python initialization:
   - Py_Initialize()
   - Import stdlib modules (encodings, etc.)
   - Import user modules (numpy, etc.)
   - Execute top-level code
4. Capture memory snapshot
5. Use component-init-transform to bake snapshot into component
6. New component starts with Python already initialized
```

### The component-init-transform Library

This is the key tool: https://github.com/bytecodealliance/component-init-transform

It instruments a component to:
1. Add `__component_init()` function that captures memory
2. Run the component with a stub environment
3. Call `__component_init()` to get the memory snapshot
4. Create a new component with that memory as the initial state

```rust
use component_init_transform::initialize_staged;

let preinit_component = initialize_staged(
    linked_component,
    None,  // No stage2
    |instrumented| async {
        // Create engine and instantiate
        let engine = Engine::new(&config)?;
        let component = Component::from_binary(&engine, instrumented)?;

        // Create minimal WASI context
        let mut store = Store::new(&engine, ctx);
        let instance = linker.instantiate_async(&mut store, &component).await?;

        // Return invoker for component-init-transform
        Ok(Box::new(PreInitInvoker { store, instance }))
    }
).await?;
```

### Our Integration Plan

#### Existing Implementation

The `feat/late-linking-exploration` branch has `preinit.rs` with:
- `PreInitCtx` - WASI context for pre-init
- `PreInitInvoker` - Implements `component_init_transform::Invoker`
- `pre_initialize()` - Main function

We can port this directly.

#### What Gets Pre-Initialized

For numpy specifically:

```python
# In the instrumented component, this runs during pre-init:
import sys
sys.path = ['/python-stdlib', '/site-packages']

# Import critical modules
import encodings          # Required for Python
import _multiarray_umath  # numpy's core native extension
import numpy              # Load numpy Python code

# After pre-init:
# - Python interpreter is initialized
# - encodings module is loaded
# - numpy is imported and ready
# - Memory contains all of the above
```

#### Memory Snapshot Size

Typical pre-init memory state:
- Empty Python interpreter: ~5MB
- With numpy imported: ~15-20MB
- Additional per import: Variable

This is embedded in the component, increasing size by that amount.

### API Design

```rust
impl SandboxBuilder {
    /// Enable pre-initialization for late-linked components.
    ///
    /// This captures Python's initialized memory state, avoiding
    /// initialization cost at runtime. Increases component size
    /// by ~15-20MB but reduces cold start from ~500ms to ~16ms.
    pub fn with_preinit(mut self, enabled: bool) -> Self {
        self.preinit_enabled = enabled;
        self
    }

    /// Specify imports to pre-initialize.
    ///
    /// These modules will be imported during pre-init and their
    /// state will be captured. Only useful with with_preinit(true).
    pub fn with_preinit_imports(mut self, imports: Vec<String>) -> Self {
        self.preinit_imports = imports;
        self
    }
}

// Usage
let sandbox = Sandbox::builder()
    .with_native_extension("numpy/core/*.so", bytes)
    .with_preinit(true)
    .with_preinit_imports(vec!["numpy".to_string()])
    .build()
    .await?;  // ← Now async because pre-init needs to run component
```

### Pre-Init Process Flow

```
┌─────────────────────────────────────────────────────────────┐
│ 1. User calls .build() with preinit enabled                 │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. Link component with extensions                           │
│    → linked_component (31MB base + 26MB numpy = 57MB)       │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. component_init_transform::initialize_staged()            │
│    a. Instrument component (add __component_init)           │
│    b. Instantiate in temporary wasmtime                     │
│    c. Run Python initialization                             │
│    d. Capture memory snapshot                               │
│    e. Embed snapshot in new component                       │
│                                                              │
│    Output: preinit_component (57MB + 15MB memory = 72MB)    │
│    Time: ~2-3 seconds                                        │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. Pre-compile the component                                │
│    wasmtime::Module::serialize()                            │
│                                                              │
│    Output: preinit_component.cwasm (ready for instant load) │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ 5. Return PythonExecutor                                     │
│    Next execute() call: ~16ms (pre-compiled + pre-init)     │
└─────────────────────────────────────────────────────────────┘
```

### Challenges

1. **WASI Context Requirements**

Pre-init needs a dummy WASI context with proper mounts:

```rust
let temp_stdlib = create_temp_stdlib()?;  // Copy stdlib to temp dir
let temp_site = create_temp_site(extensions)?;  // Extract extensions

let wasi = WasiCtxBuilder::new()
    .preopened_dir(Dir::open(&temp_stdlib)?, "/python-stdlib")?
    .preopened_dir(Dir::open(&temp_site)?, "/site-packages")?
    .build();
```

2. **Callback Stubs**

The `invoke` import must be implemented during pre-init, but no real callbacks:

```rust
// Stub implementation for pre-init
async fn stub_invoke(name: String, args: String) -> Result<String, String> {
    Err(format!("Callback '{name}' not available during pre-init"))
}
```

3. **Import Execution**

We need to tell Python what to import:

```python
# Injected during pre-init
import sys
sys.path = ['/python-stdlib', '/site-packages']

# User-specified imports
import numpy
import pandas
# etc.
```

This could be done via:
- Temporary Python file executed during pre-init
- Direct PyRun_SimpleString call
- Environment variable that runtime reads

### Implementation Steps

1. Port `preinit.rs` from `feat/late-linking-exploration`
2. Add `component-init-transform` dependency
3. Add `with_preinit()` and `with_preinit_imports()` to `SandboxBuilder`
4. Make `build()` async when pre-init is enabled
5. Handle temporary directories for VFS mounts
6. Add stub callback implementations

**Estimated time:** 3-4 days

**Performance gain:** 500ms → <16ms (30x faster)

---

## Combined Approach

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ First sandbox with numpy (cold):                             │
│   1. Link with extensions                ~1000ms             │
│   2. Pre-initialize Python                ~2000ms            │
│   3. Pre-compile to native                 ~500ms            │
│   4. Cache (key = extension hash)           ~50ms            │
│   Total: ~3550ms                                             │
└─────────────────────────────────────────────────────────────┘
                        │
                        │ Saves to cache:
                        │   /tmp/eryx-cache/<hash>.cwasm
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Second sandbox with same numpy (warm):                       │
│   1. Compute cache key                      ~1ms             │
│   2. Load from cache                        ~10ms            │
│   3. Deserialize pre-compiled               ~5ms             │
│   4. WASI context setup                     ~5ms             │
│   Total: ~21ms                                               │
└─────────────────────────────────────────────────────────────┘
```

### Performance Summary

| Scenario | Current | Level 1 Cache (bytes) | Level 2 Cache (pre-compiled) | Pre-init | Both |
|----------|---------|----------------------|------------------------------|----------|------|
| Base runtime (no numpy) | 514ms | 514ms | **10ms** | 10ms | **10ms** |
| First numpy sandbox | 991ms | 991ms | 991ms | ~3000ms | ~3000ms |
| Second numpy sandbox | 991ms | ~500ms | **10ms** | ~3000ms | **10ms** |
| Third numpy sandbox | 991ms | ~500ms | **10ms** | ~3000ms | **10ms** |

**Actual measurements (release mode):**
- Base runtime from file: 514ms (criterion benchmark)
- Embedded runtime (pre-compiled): **10.1ms** (embedded_runtime example)
- Late-linked numpy: 991ms (numpy_native example)

**Key insight:** Pre-compiled caching (Level 2) gives 100x speedup. Pre-init is expensive on first run (~3s) but subsequent runs are still fast (~10ms).

### API

```rust
use eryx::{Sandbox, ComponentCache};

// Create a persistent cache
let cache = ComponentCache::new("/tmp/eryx-cache")
    .with_max_size_bytes(1_000_000_000)  // 1GB max
    .with_eviction_policy(EvictionPolicy::LRU);

// First call: ~3.5s (link + preinit + cache)
let sandbox1 = Sandbox::builder()
    .with_native_extensions_from_wheel("/tmp/numpy-wasi.tar.gz")?
    .with_python_stdlib("/path/to/stdlib")
    .with_preinit(true)
    .with_preinit_imports(vec!["numpy".to_string()])
    .with_cache(cache.clone())
    .build()
    .await?;

// Second call: ~16ms (cache hit)
let sandbox2 = Sandbox::builder()
    .with_native_extensions_from_wheel("/tmp/numpy-wasi.tar.gz")?
    .with_python_stdlib("/path/to/stdlib")
    .with_preinit(true)
    .with_preinit_imports(vec!["numpy".to_string()])
    .with_cache(cache.clone())
    .build()
    .await?;
```

---

## Implementation Plan

### Phase 1: Pre-Compiled Component Caching (Simpler, Higher ROI)

**Estimated time:** 1-2 days

**Recommendation: Cache pre-compiled components (.cwasm), not just linked bytes (.wasm)**

This gives 100x speedup vs 2x speedup.

**Steps:**

1. **Add ComponentCache trait**
   ```rust
   // In eryx/src/cache.rs
   pub trait ComponentCache: Send + Sync {
       /// Get pre-compiled component bytes
       fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>>;
       /// Put pre-compiled component bytes
       fn put(&self, key: [u8; 32], precompiled: Vec<u8>);
   }

   pub struct FilesystemCache {
       cache_dir: PathBuf,
       max_size_bytes: u64,
       eviction: EvictionPolicy,
   }

   pub struct InMemoryCache {
       cache: Arc<Mutex<HashMap<[u8; 32], Vec<u8>>>>,
       max_entries: usize,
   }

   pub struct NoCache;  // Disables caching
   ```

2. **Update SandboxBuilder::build() to use pre-compiled caching**
   ```rust
   pub fn build(self) -> Result<Sandbox, Error> {
       #[cfg(feature = "native-extensions")]
       let executor = if !self.native_extensions.is_empty() {
           // Check cache first
           if let Some(cache) = &self.cache {
               let key = compute_cache_key(&self.native_extensions);
               if let Some(precompiled) = cache.get(&key) {
                   // Cache hit - load pre-compiled (fast!)
                   return unsafe {
                       PythonExecutor::from_precompiled(&precompiled)?
                   };
               }
           }

           // Cache miss - link and compile
           let component_bytes =
               eryx_runtime::linker::link_with_extensions(&self.native_extensions)?;

           // Create executor from linked bytes (compiles to native)
           let executor = PythonExecutor::from_binary(&component_bytes)?;

           // Pre-compile and cache for next time
           if let Some(cache) = &self.cache {
               let precompiled = executor.precompile()?;
               let key = compute_cache_key(&self.native_extensions);
               cache.put(key, precompiled);
           }

           executor
       } else {
           self.build_executor_from_source()?
       };

       // ... rest of build
   }
   ```

3. **Add to SandboxBuilder**
   ```rust
   impl SandboxBuilder {
       pub fn with_cache(mut self, cache: Arc<dyn ComponentCache>) -> Self {
           self.cache = Some(cache);
           self
       }

       pub fn with_cache_dir(self, path: impl AsRef<Path>) -> Self {
           self.with_cache(Arc::new(FilesystemCache::new(path)))
       }
   }
   ```

4. **Test caching works**
   ```rust
   #[tokio::test]
   async fn test_caching_speedup() {
       let cache = ComponentCache::new("/tmp/test-cache");

       // First call
       let start = Instant::now();
       let sandbox1 = build_with_numpy(&cache)?;
       let cold_time = start.elapsed();

       // Second call
       let start = Instant::now();
       let sandbox2 = build_with_numpy(&cache)?;
       let warm_time = start.elapsed();

       assert!(warm_time < cold_time / 50);  // At least 50x faster
   }
   ```

### Phase 2: Pre-Initialization (More Complex)

**Estimated time:** 3-4 days

**Steps:**

1. **Port preinit.rs from late-linking branch**
   - Copy `crates/eryx-runtime/src/preinit.rs`
   - Add `component-init-transform` dependency
   - Add `wasmtime` as optional dependency (only for pre-init)

2. **Create pre-init Python script generator**
   ```rust
   fn generate_preinit_script(imports: &[String]) -> String {
       format!(r#"
   import sys
   sys.path = ['/python-stdlib', '/site-packages']

   # Import specified modules
   {}
   "#, imports.iter().map(|i| format!("import {i}")).collect::<Vec<_>>().join("\n"))
   }
   ```

3. **Update linker to support pre-init**
   ```rust
   pub async fn link_with_extensions_and_preinit(
       extensions: &[NativeExtension],
       preinit_imports: &[String],
       cache: Option<&dyn ComponentCache>,
   ) -> Result<Vec<u8>, LinkError> {
       // Check cache first
       let cache_key = compute_preinit_cache_key(extensions, preinit_imports);
       if let Some(cached) = cache.and_then(|c| c.get(&cache_key)) {
           return Ok(cached);
       }

       // Link component
       let linked = link_with_extensions(extensions, None)?;

       // Pre-initialize
       let preinit = crate::preinit::pre_initialize(
           &linked,
           preinit_imports,
       ).await?;

       // Cache the pre-initialized component
       if let Some(cache) = cache {
           cache.put(cache_key, preinit.clone());
       }

       Ok(preinit)
   }
   ```

4. **Make SandboxBuilder::build() async**

   This is a breaking change - need to consider carefully:
   ```rust
   // Option A: Always async
   pub async fn build(self) -> Result<Sandbox, Error> { ... }

   // Option B: Separate method
   pub fn build(self) -> Result<Sandbox, Error> { ... }
   pub async fn build_with_preinit(self) -> Result<Sandbox, Error> { ... }

   // Option C: Conditional async
   #[cfg(feature = "pre-init")]
   pub async fn build(self) -> Result<Sandbox, Error> { ... }

   #[cfg(not(feature = "pre-init"))]
   pub fn build(self) -> Result<Sandbox, Error> { ... }
   ```

   **Recommendation:** Option B (separate method) to avoid breaking existing code.

5. **Handle temporary VFS mounts**

   Pre-init needs real directories:
   ```rust
   // Create temp dirs with stdlib and site-packages
   let temp_dir = TempDir::new()?;
   std::fs::create_dir_all(temp_dir.path().join("python-stdlib"))?;
   std::fs::create_dir_all(temp_dir.path().join("site-packages"))?;

   // Copy/symlink files
   copy_dir(&python_stdlib_path, temp_dir.path().join("python-stdlib"))?;
   copy_dir(&site_packages_path, temp_dir.path().join("site-packages"))?;

   // Use in WASI context for pre-init
   let wasi = WasiCtxBuilder::new()
       .preopened_dir(temp_dir.path().join("python-stdlib"), "/python-stdlib")?
       .preopened_dir(temp_dir.path().join("site-packages"), "/site-packages")?
       .build();
   ```

6. **Test pre-init works**
   ```rust
   #[tokio::test]
   async fn test_preinit_speedup() {
       // Without pre-init
       let start = Instant::now();
       let sandbox1 = build_numpy_without_preinit()?;
       let result1 = sandbox1.execute("import numpy").await?;
       let no_preinit_time = start.elapsed();

       // With pre-init
       let start = Instant::now();
       let sandbox2 = build_numpy_with_preinit().await?;
       let result2 = sandbox2.execute("import numpy").await?;
       let preinit_time = start.elapsed();

       assert!(preinit_time < no_preinit_time / 10);  // 10x faster
   }
   ```

### Challenges

1. **Async Build Method**

   Pre-init requires running the component, which is async. Options:
   - Make `build()` async (breaking change)
   - Add `build_async()` method (non-breaking)
   - Add `build_with_preinit()` method (non-breaking)

2. **Temporary Directories**

   VFS mounts need real directories. Must:
   - Create temp dirs for pre-init
   - Copy stdlib and site-packages
   - Clean up after pre-init completes

3. **Import Ordering**

   Some packages must be imported in specific order:
   ```python
   import numpy  # Must come before pandas
   import pandas
   ```

   Need to preserve user-specified order.

4. **Callback Stubs**

   During pre-init, callbacks aren't real. Need stubs:
   ```rust
   async fn preinit_stub_callback(name: String, _args: String)
       -> Result<String, String>
   {
       Err(format!("Callback '{name}' not available during pre-init"))
   }
   ```

5. **Determinism**

   Pre-init must be deterministic for caching:
   - Same extensions + same imports = same memory state
   - Non-deterministic imports (random, time.time()) break this

---

## Recommended Implementation Order

### Week 1: Caching (High ROI, Low Risk)

- Day 1-2: Implement `ComponentCache` trait and filesystem backend
- Day 3: Integrate with `SandboxBuilder`
- Day 4: Add tests and benchmarks
- Day 5: Buffer time

**Result:** 100x speedup for repeated extension combinations

### Week 2: Pre-Initialization (Higher Complexity)

**Note:** Implementation already exists in `feat/late-linking-exploration` branch at `crates/eryx-runtime/src/preinit.rs` (~410 lines). This includes:
- `PreInitCtx` - WASI context implementation
- `PreInitInvoker` - Implements `component_init_transform::Invoker` trait
- `pre_initialize()` - Main async function that runs component init

Just needs porting and integration with our current linker.

**Steps:**

- Day 1: Port `preinit.rs` from `feat/late-linking-exploration` branch
- Day 2: Integrate with linker and builder
- Day 3: Handle temporary directories and VFS setup
- Day 4-5: Testing, benchmarks, edge cases

**Result:** 30x speedup even on first run (combined with caching)

### Combined Result

After both optimizations:

```
Scenario: Create 10 sandboxes with numpy

Current (measured):
  10 × 991ms = 9,910ms (~10 seconds)

With Level 1 caching (linked bytes):
  1 × 991ms + 9 × 500ms = 5,491ms (~5.5 seconds)
  Saves linking but still compiles each time

With Level 2 caching (pre-compiled - RECOMMENDED):
  1 × 991ms + 9 × 10ms = 1,081ms (~1.1 seconds)
  Saves linking AND compilation

With pre-init only (no caching):
  10 × 3000ms = 30,000ms (~30 seconds - WORSE!)
  Each sandbox runs full pre-init process

With Level 2 cache + pre-init:
  1 × 3000ms + 9 × 10ms = 3,090ms (~3.1 seconds)
  First is slow, rest are fast

But if cache is warm (best case):
  10 × 10ms = 100ms (0.1 seconds - 100x faster!)
```

**Key insight:** Pre-compiled caching (Level 2) is the high-leverage optimization, giving 100x speedup. Pre-init is nice for first-run experience but adds ~2s overhead, so only worthwhile if you create many sandboxes.

---

## Alternative: Lazy Pre-Init

Instead of pre-initializing during build, do it on first execute:

```rust
struct LazyPreInitExecutor {
    base_executor: PythonExecutor,
    preinit_state: Arc<Mutex<Option<Vec<u8>>>>,  // Captured memory
}

impl LazyPreInitExecutor {
    async fn execute(&self, code: &str) -> Result<ExecuteResult> {
        // First call: capture state after initialization
        if self.preinit_state.lock().unwrap().is_none() {
            let result = self.base_executor.execute(code).await?;
            let state = self.base_executor.snapshot_state()?;
            *self.preinit_state.lock().unwrap() = Some(state);
            return Ok(result);
        }

        // Subsequent calls: restore from snapshot
        self.base_executor.restore_state(&snapshot)?;
        self.base_executor.execute(code).await
    }
}
```

**Pros:**
- No async build method needed
- No temporary directories
- Simpler implementation

**Cons:**
- First execute() is still slow
- snapshot/restore adds overhead (~10-20ms per execution)
- Memory state grows over time

**Verdict:** Not recommended. True pre-init is better.

---

## Open Questions

1. **Should pre-init be default when using native extensions?**

   Current: Opt-in via `with_preinit(true)`
   Alternative: Default to true, opt-out via `with_preinit(false)`

2. **Where should cache directory default to?**

   Options:
   - `$HOME/.cache/eryx/components/`
   - `$TMPDIR/eryx-cache/`
   - No default (user must specify)

3. **Should we bundle Python stdlib in the component?**

   Pros: Simpler API (no `with_python_stdlib()` needed)
   Cons: +50MB to every component

   Alternative: Small essential-only stdlib (~5MB)

4. **Should caching be enabled by default?**

   With filesystem cache in `$HOME/.cache/eryx/`:
   - Pros: Automatic speedup, no user action needed
   - Cons: Hidden disk usage, potential permission issues

5. **How to handle preinit failures?**

   If import fails during pre-init:
   - Fail the entire build()?
   - Fall back to non-pre-init?
   - Warn and continue?

---

## Existing preinit.rs Implementation

### Location

`feat/late-linking-exploration` branch:
- File: `crates/eryx-runtime/src/preinit.rs` (~410 lines)
- Commit: `71d96ac` "feat: Late-linking exploration for native Python extensions"

### What's Already Implemented

```rust
// PreInitCtx - WASI context for pre-init
struct PreInitCtx {
    wasi: WasiCtx,
    table: ResourceTable,
    app_dir: TempDir,  // Keeps temp dir alive
}

impl WasiView for PreInitCtx { ... }

// PreInitInvoker - Implements component-init-transform's Invoker trait
struct PreInitInvoker {
    store: Store<PreInitCtx>,
    instance: Instance,
}

#[async_trait]
impl Invoker for PreInitInvoker {
    async fn call_s32(&mut self, function: &str) -> Result<i32> { ... }
    async fn call_s64(&mut self, function: &str) -> Result<i64> { ... }
    async fn call_f32(&mut self, function: &str) -> Result<f32> { ... }
    async fn call_f64(&mut self, function: &str) -> Result<f64> { ... }
    async fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>> { ... }
}

// Main function
pub async fn pre_initialize(
    component: &[u8],
    app_name: &str
) -> Result<Vec<u8>> {
    component_init_transform::initialize_staged(
        component,
        None,
        |instrumented| async {
            // Setup engine and WASI context
            // Instantiate component
            // Return PreInitInvoker
        }
    ).await
}
```

### What Needs to Be Added

1. **Python import injection**: The existing impl takes `app_name` but doesn't specify what to import. Need to add parameter for imports:
   ```rust
   pub async fn pre_initialize(
       component: &[u8],
       imports: &[String],  // ← Add this
   ) -> Result<Vec<u8>>
   ```

2. **VFS setup**: Need to mount Python stdlib and site-packages for pre-init. The existing impl uses temp dirs but doesn't populate them.

3. **Integration with linker**: Connect `link_with_extensions()` → `pre_initialize()` → `cache.put()`

### Porting Strategy

1. Copy `preinit.rs` from `feat/late-linking-exploration` to `crates/eryx-runtime/src/`
2. Update function signature to take imports
3. Add VFS directory population logic
4. Update dependencies in `Cargo.toml` (component-init-transform, etc.)
5. Add feature flag `pre-init` to gate the functionality
6. Test with numpy imports

---

## References

- [component-init-transform](https://github.com/bytecodealliance/component-init-transform) - Pre-initialization tool
- [feat/late-linking-exploration @ 71d96ac](../../tree/feat/late-linking-exploration) - Existing preinit.rs implementation
- [componentize-py pre-init](https://github.com/bytecodealliance/componentize-py/blob/main/src/componentize_py/_init.py) - Reference implementation
- [plans/PROGRESS.md](../../plans/PROGRESS.md) - Main branch benchmark results (1.57ms per-execution with componentize-py)
