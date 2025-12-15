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
