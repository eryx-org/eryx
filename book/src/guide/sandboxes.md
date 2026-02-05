# Sandboxes

Sandboxes are the core primitive in Eryx. A sandbox provides an isolated Python execution environment powered by WebAssembly, ensuring complete isolation from the host system.

## Creating a Sandbox

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Create a sandbox with embedded runtime (recommended)
    let sandbox = Sandbox::embedded().build()?;

    // Execute code
    let result = sandbox.execute("print('Hello!')").await?;
    println!("{}", result.stdout);

    Ok(())
}
```

```python
import eryx

# Create a sandbox
sandbox = eryx.Sandbox()

# Execute code
result = sandbox.execute("print('Hello!')")
print(result.stdout)  # "Hello!"
```
<!-- langtabs-end -->

## Sandbox Isolation

Each sandbox runs in complete isolation:

- **Memory isolation**: No access to host memory
- **Filesystem isolation**: No access to host filesystem (see [VFS](./vfs.md) for virtual filesystem)
- **Network isolation**: Disabled by default (see [Networking](./networking.md) to enable)
- **Process isolation**: Cannot spawn processes or execute system commands

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    // Host filesystem is not accessible
    let result = sandbox.execute(r#"
import os
try:
    files = os.listdir('/etc')
    print(f"Found {len(files)} files")
except Exception as e:
    print(f"Access blocked: {type(e).__name__}")
    "#).await?;

    println!("{}", result.stdout);
    // Output: "Access blocked: FileNotFoundError" or shows empty virtual directory

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

# Host filesystem is not accessible
result = sandbox.execute("""
import os
try:
    files = os.listdir('/etc')
    print(f"Found {len(files)} files")
except Exception as e:
    print(f"Access blocked: {type(e).__name__}")
""")

print(result.stdout)
# Output: "Access blocked: FileNotFoundError" or shows empty virtual directory
```
<!-- langtabs-end -->

## Execution Results

When you execute code, you get back an `ExecuteResult` with useful information:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    let result = sandbox.execute("x = sum(range(1000))").await?;

    println!("stdout: {}", result.stdout);
    println!("Duration: {:?}", result.stats.duration);
    println!("Peak memory: {:?} bytes", result.stats.peak_memory_bytes);
    println!("Fuel consumed: {:?}", result.stats.fuel_consumed);

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

result = sandbox.execute("x = sum(range(1000))")

print(f"stdout: {result.stdout}")
print(f"Duration: {result.duration_ms}ms")
print(f"Peak memory: {result.peak_memory_bytes} bytes")
print(f"Callback invocations: {result.callback_invocations}")
```
<!-- langtabs-end -->

## Error Handling

Sandbox execution can fail for various reasons. Eryx provides typed errors to help you handle them:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, Error};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    match sandbox.execute("raise ValueError('oops')").await {
        Ok(result) => println!("Success: {}", result.stdout),
        Err(Error::Python { message, .. }) => {
            println!("Python error: {}", message);
        }
        Err(Error::Timeout { .. }) => {
            println!("Execution timed out");
        }
        Err(e) => println!("Other error: {}", e),
    }

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

try:
    sandbox.execute("raise ValueError('oops')")
except eryx.ExecutionError as e:
    print(f"Python error: {e}")
except eryx.TimeoutError as e:
    print(f"Timeout: {e}")
except eryx.EryxError as e:
    print(f"Other error: {e}")
```
<!-- langtabs-end -->

## Reusing Sandboxes

Sandboxes can be reused for multiple executions. Each execution starts fresh without any state from previous executions:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    // First execution
    sandbox.execute("x = 42").await?;

    // Second execution - x is NOT available (fresh state each time)
    let result = sandbox.execute(r#"
try:
    print(x)
except NameError:
    print("x is not defined")
    "#).await?;

    println!("{}", result.stdout);  // "x is not defined"

    Ok(())
}
```

```python
import eryx

sandbox = eryx.Sandbox()

# First execution
sandbox.execute("x = 42")

# Second execution - x is NOT available (fresh state each time)
result = sandbox.execute("""
try:
    print(x)
except NameError:
    print("x is not defined")
""")

print(result.stdout)  # "x is not defined"
```
<!-- langtabs-end -->

If you need state to persist across executions, use a [Session](./sessions.md).

## SandboxFactory for Fast Creation

When creating many sandboxes, use `SandboxFactory` to pre-initialize Python and packages once, then quickly instantiate sandboxes from that snapshot:

<!-- langtabs-start -->
```rust,ignore
# extern crate eryx;
# extern crate tokio;
// SandboxFactory is primarily useful in the Python bindings
// In Rust, use Sandbox::embedded() which already uses pre-compiled WASM
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    // Sandbox::embedded() is already optimized for fast creation
    for i in 0..5 {
        let sandbox = Sandbox::embedded().build()?;
        let result = sandbox.execute(&format!("print('sandbox {}')", i)).await?;
        println!("{}", result.stdout);
    }
    Ok(())
}
```

```python
import eryx

# Create a factory (takes ~2s to initialize Python)
factory = eryx.SandboxFactory()

print(f"Factory size: {factory.size_bytes} bytes")

# Create sandboxes quickly from the factory (~16ms each)
for i in range(5):
    sandbox = factory.create_sandbox()
    result = sandbox.execute(f"print('sandbox {i}')")
    print(result.stdout)
```
<!-- langtabs-end -->

### Factory with Pre-installed Packages

You can create a factory with packages pre-installed:

```python
import eryx

# Create a factory with packages
factory = eryx.SandboxFactory(
    packages=[
        "path/to/jinja2.whl",
        "path/to/markupsafe.whl",
    ],
    imports=["jinja2"],  # Pre-import these modules
)

# Sandboxes created from this factory have packages ready
sandbox = factory.create_sandbox()
result = sandbox.execute("""
from jinja2 import Template
t = Template("Hello {{ name }}")
print(t.render(name="World"))
""")
print(result.stdout)  # "Hello World"
```

### Saving and Loading Factories

Factories can be serialized for later use:

```python
import eryx
from pathlib import Path

# Create and save
factory = eryx.SandboxFactory()
factory.save(Path("factory.bin"))

# Load later
loaded = eryx.SandboxFactory.load(Path("factory.bin"))
sandbox = loaded.create_sandbox()
```

## Next Steps

- [Sessions](./sessions.md) - Maintain state across executions
- [Resource Limits](./resource-limits.md) - Control execution time and memory
- [Callbacks](./callbacks.md) - Allow sandbox code to call host functions
- [Networking](./networking.md) - Enable network access
