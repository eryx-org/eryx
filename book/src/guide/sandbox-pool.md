# Sandbox Pool

A `SandboxPool` manages a bounded set of warm sandbox instances for high-throughput concurrent execution. Instead of creating a new sandbox per request, you lease one from the pool, execute code, and return it automatically.

Use a pool when you serve concurrent requests and want to bound resource usage while avoiding per-request sandbox creation cost.

## Creating a Pool

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, SandboxPool, PoolConfig};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), eryx::PoolError> {
    let config = PoolConfig {
        max_size: 4,
        min_idle: 1,
        acquire_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(300),
    };

    let pool = SandboxPool::new(Sandbox::embedded(), config).await?;

    // Or with a custom builder for more control:
    let pool = SandboxPool::with_builder(
        || Sandbox::embedded().with_trace_collection(false).build(),
        PoolConfig::default(),
    ).await?;

    Ok(())
}
```

```python
import eryx

factory = eryx.SandboxFactory(cache=True)
pool = factory.create_pool(
    max_size=4,          # at most 4 concurrent sandboxes
    min_idle=1,          # keep 1 warm sandbox ready
    acquire_timeout_ms=5000,   # wait up to 5s for a sandbox
    idle_timeout_ms=300_000,   # evict idle sandboxes after 5min
)
```
<!-- langtabs-end -->

In Python, the pool reuses the factory's precompiled artifact and shared Tokio runtime. When the pool needs a new sandbox (because the idle queue is empty), creation is fast since the precompiled artifact avoids re-deserializing the WASM component.

## Acquiring and Releasing Leases

Acquire a sandbox, use it, and return it to the pool. The sandbox is returned automatically when the scope ends:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# use eryx::{Sandbox, SandboxPool, PoolConfig};
#
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
#     let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default()).await?;
// Acquire a sandbox from the pool
let sandbox = pool.acquire().await?;

let result = sandbox.execute("print('Hello from pool!')").await?;
println!("{}", result.stdout_text());

// Sandbox is returned to the pool when dropped
drop(sandbox);
#     Ok(())
# }
```

```python,no_test
with pool.acquire() as sandbox:
    result = sandbox.execute('print("Hello from pool!")')
    print(result.stdout_text)
# sandbox is returned to the pool here
```
<!-- langtabs-end -->

In Python, use-after-release raises `ValueError`, and repeated release is safe (idempotent):

```python,no_test
with pool.acquire() as sandbox:
    sandbox.execute('pass')

sandbox.execute('pass')  # raises ValueError: sandbox has been released

# Explicit release is also available:
sandbox = pool.acquire()
sandbox.execute('print(42)')
sandbox.release()   # returned to pool
sandbox.release()   # no-op
```

## Per-Request Configuration

Each acquisition accepts per-request callbacks, output handlers, and resource limits. These are cleared automatically on release so the next borrower gets a clean sandbox:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# use eryx::{Sandbox, SandboxPool, PoolConfig};
# use eryx::ResourceLimits;
# use std::time::Duration;
#
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
#     let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default()).await?;
let limits = ResourceLimits::default()
    .with_execution_timeout(Duration::from_secs(2));
let sandbox = pool.acquire().await?
    .with_resource_limits(limits);

let result = sandbox.execute("print('with limits')").await?;
// Limits are cleared when sandbox is returned to pool
#     Ok(())
# }
```

```python,no_test
def handle_output(chunk: bytes):
    print(f"[stdout] {chunk.decode()}", end="")

with pool.acquire(
    resource_limits=eryx.ResourceLimits(execution_timeout_ms=2000),
    on_stdout=handle_output,
    callbacks=[{"name": "greet", "fn": lambda: {"msg": "hi"}, "description": "greet"}],
) as sandbox:
    result = sandbox.execute('import json; print(json.dumps(await greet()))')
```
<!-- langtabs-end -->

If you don't pass callbacks in Python, the factory's default callbacks (baked into the snapshot) are used.

## Non-Blocking Acquire

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# use eryx::{Sandbox, SandboxPool, PoolConfig};
#
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
#     let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default()).await?;
if let Some(sandbox) = pool.try_acquire()? {
    let result = sandbox.execute("print('got one')").await?;
} else {
    println!("pool is busy");
}
#     Ok(())
# }
```

```python,no_test
sandbox = pool.try_acquire()
if sandbox is not None:
    with sandbox:
        sandbox.execute('print("got one")')
else:
    print("pool is busy")
```
<!-- langtabs-end -->

## Pool Stats and Eviction

Monitor pool usage and evict idle sandboxes that have exceeded the idle timeout (respects `min_idle`):

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# use eryx::{Sandbox, SandboxPool, PoolConfig};
#
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
#     let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default()).await?;
let stats = pool.stats();
println!("idle={}, in_use={}, total={}", stats.idle, stats.in_use, stats.total);

let evicted = pool.evict_idle();
println!("evicted {} idle sandboxes", evicted);
#     Ok(())
# }
```

```python,no_test
stats = pool.stats()
print(f"idle={stats.idle}, in_use={stats.in_use}, total={stats.total}")
print(f"acquisitions={stats.total_acquisitions}, avg_wait={stats.average_wait_time_ms:.1f}ms")

evicted = pool.evict_idle()
```
<!-- langtabs-end -->

## Closing the Pool

`close()` prevents new acquisitions, wakes any threads blocked on `acquire()`, and drops idle sandboxes. Existing leases continue to work but are not returned on release:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
# use eryx::{Sandbox, SandboxPool, PoolConfig, PoolError};
#
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
#     let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default()).await?;
pool.close();
assert!(pool.is_closed());

match pool.acquire().await {
    Err(PoolError::Closed) => println!("pool is closed"),
    _ => unreachable!(),
}
#     Ok(())
# }
```

```python,no_test
pool.close()
pool.acquire()  # raises eryx.PoolClosedError

# The pool is also a context manager:
with factory.create_pool(max_size=4) as pool:
    with pool.acquire() as sandbox:
        sandbox.execute('print("ok")')
# pool.close() called automatically
```
<!-- langtabs-end -->

## Thread Safety

The pool is safe to use from multiple threads. In Python, the GIL is released while waiting for a sandbox and during guest code execution, so other threads can make progress — including returning their own leases to unblock waiting acquirers.

```python,no_test
import threading

def worker(pool, idx):
    with pool.acquire() as sandbox:
        result = sandbox.execute(f'print("worker {idx}")')

threads = [threading.Thread(target=worker, args=(pool, i)) for i in range(8)]
for t in threads:
    t.start()
for t in threads:
    t.join()
```

## Pool Exceptions

| Exception | When |
|-----------|------|
| `PoolTimeoutError` | `acquire()` timed out waiting for a sandbox |
| `PoolClosedError` | Pool has been closed |
| `PoolExhaustedError` | All sandboxes in use (non-blocking path) |

All inherit from `PoolError` (Rust: `eryx::PoolError`, Python: `eryx.PoolError`).

## Next Steps

- [Resource Limits](./resource-limits.md) - Control execution time and memory per lease
- [Callbacks](./callbacks.md) - Register callbacks for pool sandboxes
- [Output Streaming](./output-streaming.md) - Stream stdout/stderr per lease
