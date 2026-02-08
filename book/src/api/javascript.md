# JavaScript API

Eryx provides JavaScript bindings via WebAssembly, allowing you to run sandboxed Python code in browsers and Node.js.

## Installation

```bash
npm install @bsull/eryx
```

> **Requirement:** WebAssembly JSPI (JavaScript Promise Integration) support is required. In Node.js, use `--experimental-wasm-jspi`. In browsers, Chrome 133+ and Edge 133+ are supported.

## Sandbox Class

The `Sandbox` class provides an isolated Python environment. Each instance maintains its own state across `execute()` calls.

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
```

### `execute(code: string): Promise<ExecuteResult>`

Execute Python code in the sandbox. Variables and imports persist across calls.

```javascript
await sandbox.execute('x = 42');
const result = await sandbox.execute('print(x + 1)');
console.log(result.stdout);  // "43\n"
console.log(result.stderr);  // ""
```

**Returns:** `ExecuteResult` with `stdout` and `stderr` string fields.

**Throws:** If the Python code raises an unhandled exception.

### `snapshotState(): Promise<Uint8Array>`

Capture a snapshot of the current Python session state. Returns serialized state that can be restored later.

```javascript
await sandbox.execute('x = 42');
const snapshot = await sandbox.snapshotState();
// Save snapshot for later use
```

### `restoreState(data: Uint8Array): Promise<void>`

Restore Python session state from a previously captured snapshot.

```javascript
await sandbox.restoreState(snapshot);
const result = await sandbox.execute('print(x)');
console.log(result.stdout);  // "42\n"
```

### `clearState(): Promise<void>`

Clear all persistent state from the session.

```javascript
await sandbox.execute('x = 42');
await sandbox.clearState();
// x is no longer defined
```

## Convenience Function

### `execute(code: string): Promise<ExecuteResult>`

Execute Python code using a shared global sandbox state. For isolated execution, create a `Sandbox` instance instead.

```javascript
import { execute } from "@bsull/eryx";

const result = await execute('print("Hello!")');
console.log(result.stdout);  // "Hello!\n"
```

## Handler Functions

### `setCallbackHandler(handler)`

Register a callback handler that sandboxed Python code can invoke.

```javascript
import { setCallbackHandler, setCallbacks, Sandbox } from "@bsull/eryx";

setCallbacks([
  { name: "get_time", description: "Returns current timestamp" },
]);

setCallbackHandler((name, argsJson) => {
  if (name === "get_time") {
    return JSON.stringify({ timestamp: Date.now() });
  }
  throw new Error(`Unknown callback: ${name}`);
});
```

> **Note:** Due to a limitation in jco 1.16.1, async import lowering for callbacks is not yet functional. Callback handlers are registered but will not be invoked at runtime. This will be resolved in a future release.

### `setOutputHandler(handler)`

Set a handler for streaming stdout/stderr output in real-time.

```javascript
import { setOutputHandler } from "@bsull/eryx";

setOutputHandler((stream, data) => {
  // stream: 0 = stdout, 1 = stderr
  if (stream === 0) {
    console.log("[stdout]", data);
  } else {
    console.error("[stderr]", data);
  }
});
```

> **Note:** Output handlers have the same jco 1.16.1 limitation as callback handlers.

### `setTraceHandler(handler)`

Set a handler for execution trace events.

```javascript
import { setTraceHandler } from "@bsull/eryx";

setTraceHandler((lineno, eventJson, contextJson) => {
  const event = JSON.parse(eventJson);
  console.log(`Line ${lineno}: ${event.type}`);
});
```

> **Note:** Trace handlers have the same jco 1.16.1 limitation as callback handlers.

## Browser Usage

In browsers, import the package normally. The WASM binary and Python stdlib are loaded automatically.

```html
<script type="module">
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute('print("Hello from the browser!")');
document.getElementById("output").textContent = result.stdout;
</script>
```

A live demo is available at [eryx.bsull.dev](https://eryx.bsull.dev).

### Browser Requirements

- Chrome 133+ or Edge 133+ (WebAssembly JSPI support)
- The stdlib tarball (`python-stdlib.tar.gz`) is fetched and decompressed on first load

## Node.js Usage

Node.js requires the `--experimental-wasm-jspi` flag:

```bash
node --experimental-wasm-jspi your-script.mjs
```

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute(`
import json
data = {"message": "Hello from Node.js!"}
print(json.dumps(data, indent=2))
`);
console.log(result.stdout);
```

## ExecuteResult

```typescript
interface ExecuteResult {
  /** Captured standard output */
  stdout: string;
  /** Captured standard error */
  stderr: string;
}
```

## Next Steps

- [Installation](../getting-started/installation.md) — Install instructions for all platforms
- [Python API](./python.md) — Python bindings reference
- [Rust API](./rust.md) — Rust API reference
