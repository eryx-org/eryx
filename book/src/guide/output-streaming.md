# Output Streaming

By default, Eryx captures stdout and stderr and returns them in the `ExecuteResult` after execution completes. Output streaming lets you receive output in real-time as Python code runs, which is useful for long-running scripts, progress reporting, and interactive interfaces.

## Python API

Pass `on_stdout` and/or `on_stderr` callbacks when creating a sandbox or session:

```python
import eryx

def handle_stdout(chunk: str) -> None:
    print(f"[stdout] {chunk}", end="")

def handle_stderr(chunk: str) -> None:
    print(f"[stderr] {chunk}", end="")

sandbox = eryx.Sandbox(
    on_stdout=handle_stdout,
    on_stderr=handle_stderr,
)

sandbox.execute("""
import sys
print("Hello from stdout")
print("Warning!", file=sys.stderr)
for i in range(5):
    print(f"Step {i}...")
""")
```

Output handlers also work with sessions:

```python
session = eryx.Session(
    on_stdout=lambda chunk: print(chunk, end=""),
    on_stderr=lambda chunk: print(chunk, end="", file=sys.stderr),
)
```

### Callback Signature

Both `on_stdout` and `on_stderr` receive a single `str` argument — the text chunk written by Python. Chunks may be partial lines or multiple lines, depending on how the sandboxed code writes output.

## Rust API

Implement the `OutputHandler` trait and pass it to the sandbox builder:

```rust,ignore
use eryx::{Sandbox, OutputHandler};
use async_trait::async_trait;

struct MyOutputHandler;

#[async_trait]
impl OutputHandler for MyOutputHandler {
    async fn on_output(&self, chunk: &str) {
        print!("{chunk}");
    }

    async fn on_stderr(&self, chunk: &str) {
        eprint!("{chunk}");
    }
}

let sandbox = Sandbox::embedded()
    .with_output_handler(MyOutputHandler)
    .build()?;
```

The `OutputHandler` trait has two methods:

- `on_output(&self, chunk: &str)` — Called for stdout output (required)
- `on_stderr(&self, chunk: &str)` — Called for stderr output (optional, default ignores stderr)

## How It Works

When an output handler is configured, Eryx sets up a channel between the WASM executor and the handler. As Python writes to `sys.stdout` or `sys.stderr`, the output is forwarded to the handler in real-time rather than being buffered until execution completes.

The final `ExecuteResult` still contains the complete captured stdout and stderr, so you get both real-time streaming and the full result.

## Use Cases

- **Progress bars** — Show progress as a long computation runs
- **Live logging** — Stream log output to a UI
- **Interactive tools** — The CLI uses output streaming for real-time REPL output
- **Monitoring** — Watch sandbox execution in real-time

## Next Steps

- [Sandboxes](./sandboxes.md) — Sandbox configuration
- [Sessions](./sessions.md) — Stateful execution
