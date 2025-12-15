# Memory Usage Measurement Plan

**Goal:** Understand memory overhead of multiple concurrent sandboxes and identify optimization opportunities.

## Key Questions

1. **Component sharing**: Does wasmtime share the compiled component bytes across instances?
2. **Per-sandbox overhead**: How much memory does each additional sandbox consume?
3. **Linear memory**: How much WASM linear memory does each instance allocate?
4. **Python state**: How much memory does Python's initialized state consume?
5. **Scaling**: Does memory grow linearly with sandbox count, or are there shared resources?

## Measurement Approach

### 1. Baseline Measurements

Create a benchmark that:
- Measures process RSS (Resident Set Size) before/after creating sandboxes
- Creates N sandboxes (1, 10, 50, 100) and measures memory at each step
- Distinguishes between:
  - Component loading (should be shared)
  - Instance creation (per-sandbox)
  - Python initialization (per-sandbox)

### 2. Tools

```rust
// Get current process RSS
fn get_rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            let kb: f64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}
```

### 3. Test Scenarios

#### Scenario A: Base runtime (no numpy)
- Measure memory with embedded runtime
- Create 1, 10, 50, 100 sandboxes
- Calculate per-sandbox overhead

#### Scenario B: With numpy (pre-initialized)
- Measure memory with numpy pre-init component
- Create 1, 10, 50, 100 sandboxes
- Compare per-sandbox overhead to base

#### Scenario C: Session reuse vs new sandboxes
- Compare memory: 1 sandbox with 100 executions vs 100 sandboxes
- Validate session reuse is memory-efficient

### 4. Expected Results

Based on wasmtime architecture:
- **Component code**: Should be shared (mmap'd, CoW)
- **Linear memory**: Per-instance (~256MB virtual, but sparse)
- **WASI state**: Per-instance (file descriptors, env, etc.)
- **Python heap**: Per-instance (within linear memory)

### 5. Wasmtime Memory Model

```
┌─────────────────────────────────────────────────────────────┐
│ Process Memory                                               │
├─────────────────────────────────────────────────────────────┤
│ Shared (mmap'd, read-only):                                 │
│   - Compiled component code (.cwasm)                        │
│   - Pre-initialized memory image (CoW)                      │
├─────────────────────────────────────────────────────────────┤
│ Per-Instance:                                                │
│   - WASM linear memory (4GB virtual, sparse allocation)     │
│   - Instance metadata (~KB)                                  │
│   - WASI resources (file handles, etc.)                     │
│   - Rust-side state (ExecutorState, channels, etc.)         │
└─────────────────────────────────────────────────────────────┘
```

### 6. Implementation Plan

1. Create `examples/memory_bench.rs` with RSS tracking
2. Test base runtime memory scaling
3. Test numpy runtime memory scaling
4. Compare with/without CoW (`memory_init_cow`)
5. Document findings and identify optimization opportunities

### 7. Potential Optimizations

If memory overhead is high:
- **Instance pooling**: Reuse instances instead of creating new ones
- **Memory limits**: Configure smaller linear memory limits
- **Lazy allocation**: Ensure wasmtime uses sparse memory allocation
- **Component deduplication**: Verify component bytes are truly shared

## Success Criteria

- Understand per-sandbox memory cost
- Confirm component sharing works
- Identify any memory leaks or unexpected growth
- Document recommended limits for concurrent sandboxes

---

## Results (December 2024)

### Key Finding: Loading Method Matters!

The way the precompiled component is loaded has a **13x impact** on memory efficiency:

| Loading Method | Per-Sandbox | 50 Sandboxes | 100 Sandboxes |
|----------------|-------------|--------------|---------------|
| **Bytes (RAM)** | ~85 MB | ~4.3 GB | ~8.5 GB (est.) |
| **Mmap (file)** | ~6.5 MB | ~326 MB | ~876 MB |

### Base Runtime (No Numpy)

```
Sandbox Count  RSS (MB)  Per-Sandbox (MB)
         1       70.1         65.1
         5      197.3         31.8
        10      356.3         31.8
        25      833.4         31.8
        50     1628.4         31.8
```

- First sandbox: ~65 MB (includes shared component loading)
- Each additional sandbox: ~32 MB
- Total for 50 sandboxes: ~1.6 GB

### Numpy Runtime with Bytes Loading

```
Sandbox Count  RSS (MB)  Per-Sandbox (MB)
         1     1152.4         82.5
         5     1480.7         82.1
        10     1891.1         82.1
        25     3151.3         84.0
        50     5352.7         85.7
```

- Per sandbox: ~85 MB
- 50 sandboxes: ~4.3 GB overhead
- Component NOT shared when loaded from bytes

### Numpy Runtime with Mmap Loading (RECOMMENDED)

```
Sandbox Count  RSS (MB)  Per-Sandbox (MB)
         1     1069.9          4.6
         5     1085.9          4.0
        10     1105.8          4.0
        25     1194.4          5.9
        50     1443.4         10.0
       100     1941.4          8.8
```

- Per sandbox: ~4-10 MB
- 100 sandboxes: ~876 MB overhead
- **13x more memory efficient than bytes loading**

### Analysis

1. **Component Sharing Works with Mmap**: When using `with_precompiled_file()` or `with_cache_dir()`, wasmtime memory-maps the compiled component, allowing it to be shared across instances via the OS page cache.

2. **Bytes Loading Defeats Sharing**: When using `with_precompiled_bytes()`, each sandbox deserializes its own copy of the component into memory.

3. **Per-Instance Overhead**: The true per-instance cost is ~4-10 MB, which includes:
   - Instance metadata (~KB)
   - WASI resources
   - Rust-side state (channels, etc.)
   - Copy-on-write pages that have been modified

4. **Virtual Memory**: VSZ is high (~8-13 GB for 100 sandboxes) but this is virtual, not physical. WASM linear memory is sparse.

### Recommendations

1. **Always use file-based loading** for production:
   ```rust
   Sandbox::builder()
       .with_cache_dir("/var/cache/eryx")?  // Enables mmap
       .build()?
   ```

2. **Session reuse is even better**: For multiple executions, reuse a session (~0.9ms per execution) instead of creating new sandboxes.

3. **Concurrent sandbox limits**:
   - With mmap: ~1000 sandboxes per GB of RAM
   - With bytes: ~12 sandboxes per GB of RAM

4. **Memory budget guidance**:
   | Available RAM | Max Sandboxes (mmap) | Max Sandboxes (bytes) |
   |---------------|---------------------|----------------------|
   | 4 GB          | ~400                | ~50                  |
   | 8 GB          | ~900                | ~95                  |
   | 16 GB         | ~1800               | ~190                 |

### Potential Future Optimizations

1. **Shared Engine**: Currently each `PythonExecutor` creates its own wasmtime `Engine`. Sharing an Engine across sandboxes could reduce memory and improve startup time since Engine creation is expensive.

2. **Instance Pooling**: Wasmtime supports instance pooling which can reuse memory allocations. This requires tuning based on workload patterns.

3. **Memory Limits**: Configure smaller WASM linear memory limits if full 4GB virtual space isn't needed.

4. **Pre-initialization Scope**: Pre-initialize with commonly used imports (like numpy) to amortize initialization cost across sandboxes.
