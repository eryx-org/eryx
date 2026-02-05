# Sessions

Sessions maintain Python state across multiple executions, enabling REPL-style interactive usage. Unlike regular sandbox executions which start fresh each time, sessions preserve variables, functions, classes, and imports.

## Creating a Session

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // Execute code in the session
    session.execute("x = 42").await?;
    let result = session.execute("print(x)").await?;
    println!("{}", result.stdout);  // "42"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

# Execute code in the session
session.execute("x = 42")
result = session.execute("print(x)")
print(result.stdout)  # "42"
```
<!-- langtabs-end -->

## State Persistence

Sessions preserve all Python state between executions:

### Variables

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("x = 42").await?;
    session.execute("y = x * 2").await?;
    let result = session.execute("print(f'{x}, {y}')").await?;
    println!("{}", result.stdout);  // "42, 84"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("x = 42")
session.execute("y = x * 2")
result = session.execute("print(f'{x}, {y}')")
print(result.stdout)  # "42, 84"
```
<!-- langtabs-end -->

### Functions and Classes

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // Define a function
    session.execute(r#"
def greet(name):
    return f"Hello, {name}!"
    "#).await?;

    // Use it later
    let result = session.execute("print(greet('World'))").await?;
    println!("{}", result.stdout);  // "Hello, World!"

    // Define a class
    session.execute(r#"
class Counter:
    def __init__(self):
        self.count = 0
    def increment(self):
        self.count += 1
        return self.count
    "#).await?;

    // Create and use instances
    session.execute("c = Counter()").await?;
    let result = session.execute("print(c.increment(), c.increment(), c.increment())").await?;
    println!("{}", result.stdout);  // "1 2 3"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

# Define a function
session.execute("""
def greet(name):
    return f"Hello, {name}!"
""")

# Use it later
result = session.execute("print(greet('World'))")
print(result.stdout)  # "Hello, World!"

# Define a class
session.execute("""
class Counter:
    def __init__(self):
        self.count = 0
    def increment(self):
        self.count += 1
        return self.count
""")

# Create and use instances
session.execute("c = Counter()")
result = session.execute("print(c.increment(), c.increment(), c.increment())")
print(result.stdout)  # "1 2 3"
```
<!-- langtabs-end -->

### Imports

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("import json").await?;
    let result = session.execute(r#"print(json.dumps({"a": 1}))"#).await?;
    println!("{}", result.stdout);  // '{"a": 1}'

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("import json")
result = session.execute('print(json.dumps({"a": 1}))')
print(result.stdout)  # '{"a": 1}'
```
<!-- langtabs-end -->

## Execution Count

Sessions track how many executions have been performed:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    println!("Count: {}", session.execution_count());  // 0

    session.execute("x = 1").await?;
    println!("Count: {}", session.execution_count());  // 1

    session.execute("y = 2").await?;
    println!("Count: {}", session.execution_count());  // 2

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

print(f"Count: {session.execution_count}")  # 0

session.execute("x = 1")
print(f"Count: {session.execution_count}")  # 1

session.execute("y = 2")
print(f"Count: {session.execution_count}")  # 2
```
<!-- langtabs-end -->

## Clearing and Resetting State

### clear_state()

Clears Python variables while keeping the session instance:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("x = 42").await?;
    session.clear_state().await?;

    // x is no longer defined
    let result = session.execute("print('x' in dir())").await?;
    println!("{}", result.stdout);  // "False"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("x = 42")
session.clear_state()

# x is no longer defined
result = session.execute("print('x' in dir())")
print(result.stdout)  # "False"
```
<!-- langtabs-end -->

### reset()

Completely resets the session by creating a new WebAssembly instance:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("x = 42").await?;
    session.reset(&[]).await?;  // Full reset

    // x is no longer defined
    let result = session.execute(r#"
try:
    print(x)
except NameError:
    print("x not defined")
    "#).await?;
    println!("{}", result.stdout);  // "x not defined"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("x = 42")
session.reset()

# x is no longer defined
result = session.execute("""
try:
    print(x)
except NameError:
    print("x not defined")
""")
print(result.stdout)  # "x not defined"
```
<!-- langtabs-end -->

## Snapshots

Sessions support snapshotting and restoring state, enabling features like checkpoints or undo:

### Taking a Snapshot

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("x = 10").await?;
    session.execute("y = 20").await?;

    // Take a snapshot
    let snapshot = session.snapshot_state().await?;
    println!("Snapshot size: {} bytes", snapshot.size());

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("x = 10")
session.execute("y = 20")

# Take a snapshot
snapshot = session.snapshot_state()
print(f"Snapshot size: {len(snapshot)} bytes")
```
<!-- langtabs-end -->

### Restoring a Snapshot

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.execute("x = 10").await?;
    let snapshot = session.snapshot_state().await?;

    // Modify the state
    session.execute("x = 999").await?;
    let result = session.execute("print(x)").await?;
    println!("{}", result.stdout);  // "999"

    // Restore the snapshot
    session.restore_state(&snapshot).await?;
    let result = session.execute("print(x)").await?;
    println!("{}", result.stdout);  // "10"

    Ok(())
}
```

```python
import eryx

session = eryx.Session()

session.execute("x = 10")
snapshot = session.snapshot_state()

# Modify the state
session.execute("x = 999")
result = session.execute("print(x)")
print(result.stdout)  # "999"

# Restore the snapshot
session.restore_state(snapshot)
result = session.execute("print(x)")
print(result.stdout)  # "10"
```
<!-- langtabs-end -->

### Snapshots Across Sessions

Snapshots can be restored in different session instances:

```python
import eryx

# Create first session and build up state
session1 = eryx.Session()
session1.execute("data = [1, 2, 3]")
session1.execute("total = sum(data)")
snapshot = session1.snapshot_state()

# Restore in a completely new session
session2 = eryx.Session()
session2.restore_state(snapshot)

result = session2.execute("print(f'{data}, {total}')")
print(result.stdout)  # "[1, 2, 3], 6"
```

## Session with VFS

Sessions can be configured with a virtual filesystem for persistent file storage. See [VFS and File Persistence](./vfs.md) for details.

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

# Files written in the session persist
session.execute("open('/data/test.txt', 'w').write('hello')")
result = session.execute("print(open('/data/test.txt').read())")
print(result.stdout)  # "hello"
```

## Execution Timeout

Sessions can have an execution timeout configured:

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    session.set_timeout(Some(Duration::from_millis(500)));

    // This will timeout
    match session.execute("while True: pass").await {
        Err(eryx::Error::Timeout { .. }) => {
            println!("Execution timed out as expected");
        }
        _ => {}
    }

    Ok(())
}
```

```python
import eryx

session = eryx.Session(execution_timeout_ms=500)

try:
    session.execute("while True: pass")
except eryx.TimeoutError:
    print("Execution timed out as expected")
```
<!-- langtabs-end -->

## Sessions with Callbacks

Sessions work with callbacks just like sandboxes:

```python
import eryx

counter = {"value": 0}

def increment():
    counter["value"] += 1
    return {"count": counter["value"]}

session = eryx.Session(
    callbacks=[
        {"name": "increment", "fn": increment, "description": "Increment counter"}
    ]
)

# Callbacks work across multiple executions
session.execute("c1 = await increment()")
session.execute("c2 = await increment()")
result = session.execute("c3 = await increment(); print(c3['count'])")
print(result.stdout)  # "3"
```

## Next Steps

- [VFS and File Persistence](./vfs.md) - Store files in sessions
- [Resource Limits](./resource-limits.md) - Control execution time and memory
- [Callbacks](./callbacks.md) - Allow sandbox code to call host functions
