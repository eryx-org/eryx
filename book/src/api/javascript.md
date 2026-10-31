# JavaScript API

Eryx provides JavaScript bindings via WebAssembly, allowing you to run sandboxed Python code in browsers and Node.js.

## Installation

```bash
npm install @bsull/eryx
```

> **Requirement:** WebAssembly JSPI (JavaScript Promise Integration) support is required. In Node.js 24–25, use `--experimental-wasm-jspi` (on by default in Node.js 26+). In browsers, Chrome 133+ and Edge 133+ are supported.

## Sandbox Class

The `Sandbox` class provides an isolated Python environment. Each instance is its own WebAssembly instance, with its own interpreter, filesystem, network connections, callback handlers and journal, and keeps its state across `execute()` calls. The compiled code is shared, but each instance has its own memory (about 20 MB idle on Node.js 24). Call `sandbox.close()` (or declare it with `using`) when you're done: it closes the sandbox's connections, terminates its Worker if it has one, and makes later calls reject. A sandbox that is only dropped is freed whenever the garbage collector gets to it.

```javascript
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
```

`new Sandbox(options)` can give the sandbox its own `callbackHandler`, `callbacks`, `traceHandler` and `outputHandler`. Any it omits follow the module-wide defaults set by the [handler functions](#handler-functions).

`timeoutMs` runs the sandbox in its own Worker (`worker_threads` in Node.js) and terminates it when a call takes longer. The call rejects with a `TimeoutError` (`executeWithJournal()` reports it as `outcome.error`), and the sandbox starts over with fresh Python state. Without it, nothing interrupts a call that never finishes. The Worker costs about 70 MB on top of the sandbox's memory.

```javascript
const sandbox = new Sandbox({ timeoutMs: 5_000 });
await sandbox.execute("while True: pass"); // rejects with TimeoutError
```

### `execute(code: string): Promise<ExecuteResult>`

Execute Python code in the sandbox. Variables and imports persist across calls.

```javascript
await sandbox.execute('x = 42');
const result = await sandbox.execute('print(x + 1)');
console.log(result.stdout);  // "43\n"
console.log(result.stderr);  // ""
```

**Returns:** `ExecuteResult` with `stdout` and `stderr` string fields, plus an
optional `result` (the script's `result` variable parsed from JSON) and
`resultError` (set when the value could not be JSON-serialized).

**Throws:** If the Python code raises an unhandled exception.

### Returning a structured result

Assign a variable named `result` and it is JSON-serialized and returned as
`result.result` — a structured channel separate from `stdout`:

```javascript
const result = await sandbox.execute('result = {"answer": 42, "items": [1, 2, 3]}');
console.log(result.result); // { answer: 42, items: [ 1, 2, 3 ] }
```

If the value is not JSON-serializable, `result.result` is `undefined` and
`result.resultError` explains why. Use `sandbox.setResultVariable("name")` to
capture a different variable name.

### `executeWithJournal(code: string, options?): Promise<ReplayOutcome>`

Execute code while journaling callback results, so a later run can replay them instead of re-invoking the callbacks. Pass a previously recorded journal as `options.journal` to replay it. A callback handler can throw `SuspendCallback` to halt execution. See [Callback Replay & Suspension](../guide/callback-replay.md).

```javascript
import { Sandbox, SuspendCallback } from "@bsull/eryx";

const first = await sandbox.executeWithJournal(code);
// Later, with the same code:
const second = await sandbox.executeWithJournal(code, { journal: first.journal });
console.log(second.replayedCallbacks);
```

`ReplayOutcome` has `result` (the `ExecuteResult`, or `undefined` on failure), `error`, `journal`, `replayedCallbacks`, and `suspended` (`{ name, argsJson, reason }` if a callback suspended). It never rejects for script failures — check `suspended`, then `error`.

`journal` is always present. It is a JSON string in the same format as the Rust `CallbackJournal`; store it and pass it back unmodified, since results are kept as their exact JSON text.

A top-level `executeWithJournal(code, options?)` convenience function is also exported.

### `snapshotState(): Promise<StateSnapshot>`

Capture a snapshot of the current Python session state. Returns `{ data, skippedGlobals }`: the serialized state (a `Uint8Array`) that can be restored later, and the names of the globals left out because they could not be serialized.

```javascript
await sandbox.execute('x = 42');
const snapshot = await sandbox.snapshotState();
console.log(snapshot.skippedGlobals);  // []
// Save snapshot.data for later use
```

### `restoreState(snapshot: StateSnapshot | Uint8Array): Promise<void>`

Restore Python session state from a previously captured snapshot, or from its `data`.

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

Execute Python code in a default sandbox, created on first use, whose state persists across calls. It shares nothing with `Sandbox` instances you create. `executeWithJournal()` and `setResultVariable()` also have module-level versions that use the same default sandbox.

```javascript
import { execute } from "@bsull/eryx";

const result = await execute('print("Hello!")');
console.log(result.stdout);  // "Hello!\n"
```

## Handler Functions

### `setCallbackHandler(handler)`

Register the default callback handler that sandboxed Python code can invoke. It applies to every sandbox not given its own `callbackHandler` option.

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

### `setOutputHandler(handler)`

Set the default handler for streaming stdout/stderr output in real-time.

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

Set the default handler for execution trace events.

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

Node.js 24–25 require the `--experimental-wasm-jspi` flag (Node.js 26+ enables JSPI by default):

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
