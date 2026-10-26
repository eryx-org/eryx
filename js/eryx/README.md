# @bsull/eryx

A Python sandbox powered by WebAssembly, for browser and Node.js.

Eryx runs **CPython 3.14** inside a WASM sandbox with full isolation — no network, no filesystem, no system access unless you explicitly allow it. Variables persist across calls, state can be snapshotted/restored, and execution can be traced line-by-line.

## Install

```bash
npm install @bsull/eryx
```

**Requirements:** Chrome 133+ or Node.js 24+ (requires [JSPI](https://v8.dev/blog/jspi) via `--experimental-wasm-jspi` in Node).

## Usage

```js
import { Sandbox } from "@bsull/eryx";

const sandbox = new Sandbox();
const result = await sandbox.execute('print("Hello from Python!")');
console.log(result.stdout); // "Hello from Python!"
```

### Returning a structured result

Assign a variable named `result` and it is JSON-serialized and returned as
`result.result` — a structured channel separate from `stdout`:

```js
const result = await sandbox.execute('result = {"answer": 42, "items": [1, 2, 3]}');
console.log(result.result); // { answer: 42, items: [ 1, 2, 3 ] }
```

If the value is not JSON-serializable, `result.result` is `undefined` and
`result.resultError` explains why (execution still succeeds). Use
`sandbox.setResultVariable("name")` to capture a different variable name.

### Persistent state

Variables, functions, and imports persist across `execute()` calls:

```js
await sandbox.execute("x = 42");
await sandbox.execute("import math");
const result = await sandbox.execute("print(math.sqrt(x))");
console.log(result.stdout); // "6.48074069840786"
```

### Snapshots

Save and restore interpreter state:

```js
await sandbox.execute("data = [1, 2, 3]");
const snapshot = await sandbox.snapshotState();

await sandbox.clearState();
await sandbox.restoreState(snapshot);

const result = await sandbox.execute("print(data)");
console.log(result.stdout); // "[1, 2, 3]"
```

### Callbacks

Let Python call back into your JavaScript:

```js
import { Sandbox, setCallbackHandler, setCallbacks } from "@bsull/eryx";

setCallbacks([
  { name: "get_weather", description: "Get current weather for a city" },
]);

setCallbackHandler((name, argsJson) => {
  const args = JSON.parse(argsJson);
  return JSON.stringify({ temp: 72, unit: "F", city: args.city });
});

const sandbox = new Sandbox();
await sandbox.execute(`
import json
from eryx_callbacks import invoke
result = json.loads(invoke("get_weather", json.dumps({"city": "NYC"})))
print(f"Temperature in {result['city']}: {result['temp']}°{result['unit']}")
`);
```

`setCallbacks()` and `setCallbackHandler()` set the defaults for every sandbox. To
give one sandbox its own, pass them as options:

```js
const sandbox = new Sandbox({
  callbacks: [{ name: "get_weather", description: "Get current weather for a city" }],
  callbackHandler: (name, argsJson) => JSON.stringify({ temp: 72 }),
});
```

`traceHandler` and `outputHandler` work the same way.

### Networking

Networking is off by default. Enable it with a host policy (same rules as the Rust crate's `NetConfig`; localhost and private ranges stay blocked unless you override `blockedHosts`, and in Node.js resolved addresses are checked against `blockedNetworks`, with `allowedNetworks` and `internalHosts` as exceptions):

```js
import { Sandbox, setNetConfig } from "@bsull/eryx";

setNetConfig({ allowedHosts: ["api.github.com", "*.example.com"] });

const sandbox = new Sandbox();
await sandbox.execute(`
import json, urllib.request
repo = json.load(urllib.request.urlopen("https://api.github.com/repos/eryx-org/eryx"))
print(repo["stargazers_count"])
`);
```

In Node.js, sandboxed code gets real TCP and TLS sockets. Browsers can't open sockets, so there each request is replayed through `fetch()`: only HTTP(S) works, one request per connection, and the server must allow cross-origin requests.

### Callback replay & suspension

Journal callback results so a re-run of the same script replays them instead of calling your handler again, and let a handler suspend execution until it can answer:

```js
import { Sandbox, SuspendCallback, setCallbackHandler } from "@bsull/eryx";

setCallbackHandler((name, argsJson) => {
  if (name === "approve" && !isApproved()) {
    throw new SuspendCallback("awaiting approval"); // halts the script
  }
  return JSON.stringify(handle(name, JSON.parse(argsJson)));
});

const sandbox = new Sandbox();
const first = await sandbox.executeWithJournal(code);
if (first.suspended) {
  save(first.journal); // a JSON string; store it as-is to resume later
}

// Later: completed callbacks replay from the journal; the rest run live.
const resumed = await sandbox.executeWithJournal(code, { journal: load() });
```

See the [Callback Replay & Suspension guide](https://docs.eryx.run/latest/guide/callback-replay.html) for the matching rules and caveats.

### Streaming output

Get stdout/stderr in real-time instead of waiting for execution to complete:

```js
import { Sandbox, setOutputHandler } from "@bsull/eryx";

setOutputHandler((stream, data) => {
  const prefix = stream === 0 ? "stdout" : "stderr";
  process.stdout.write(`[${prefix}] ${data}`);
});

const sandbox = new Sandbox();
await sandbox.execute(`
for i in range(5):
    print(f"Step {i}")
`);
```

### Timeouts

Nothing interrupts a call by default, so `while True: pass` never returns. Pass `timeoutMs` to run the sandbox in its own Worker (`worker_threads` in Node.js) that is terminated when a call runs too long:

```js
import { Sandbox, TimeoutError } from "@bsull/eryx";

const sandbox = new Sandbox({ timeoutMs: 5_000 });
try {
  await sandbox.execute("while True: pass");
} catch (e) {
  console.log(e instanceof TimeoutError); // true
}
```

The call rejects with a `TimeoutError` (or `executeWithJournal()` reports it as `outcome.error`) and the sandbox starts over with fresh Python state. Callbacks, output and networking still run on the creating thread. The Worker costs about 70 MB on top of the sandbox's own memory, and the sandbox's `_fileTree` isn't available.

### Node.js and the WASI shims

Under Node.js, eryx loads the in-memory (browser) build of `@bytecodealliance/preview2-shim`, because the Node.js build backs WASI with the real filesystem. Each sandbox gets its own in-memory filesystem, so state set through the shim's module-wide functions (such as `_setFileData()`) never reaches a sandbox.

### Vite / webpack

The package works with Vite and webpack. For Vite, you'll need cross-origin isolation headers for `SharedArrayBuffer`:

```js
// vite.config.ts
export default defineConfig({
  server: {
    headers: {
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
    },
  },
});
```

## API

### `Sandbox`

| Method | Returns | Description |
|--------|---------|-------------|
| `new Sandbox(options?)` | `Sandbox` | Create an isolated sandbox; `options` can set its own `callbackHandler`, `callbacks`, `traceHandler`, `outputHandler` and `timeoutMs` |
| `execute(code)` | `Promise<{ stdout, stderr }>` | Run Python code |
| `executeWithJournal(code, { journal? })` | `Promise<ReplayOutcome>` | Run with callback journaling/replay |
| `setResultVariable(name)` | `Promise<void>` | Change the captured result variable |
| `snapshotState()` | `Promise<Uint8Array>` | Serialize interpreter state |
| `restoreState(data)` | `Promise<void>` | Restore from snapshot |
| `clearState()` | `Promise<void>` | Reset all user state |

Each `Sandbox` is its own WebAssembly instance, with its own interpreter, filesystem, callbacks and journal; code in one can't see another's state. The compiled code is shared, but each instance has its own memory (about 20 MB idle on Node.js 24), so drop sandboxes you no longer need. The module-level `execute()`, `executeWithJournal()` and `setResultVariable()` use one default sandbox, created on first use.

Calls on one sandbox run one at a time, in call order: a call made while another is running waits for it to finish. So don't await `execute()` on the same sandbox from inside its callback handler, since it would wait on the execution that called the handler. Different sandboxes run independently.

### Callbacks

| Function | Description |
|----------|-------------|
| `setCallbackHandler(fn)` | Register the default `(name, argsJson) => resultJson` handler |
| `setCallbacks(infos)` | Register the default callbacks visible to Python's `list_callbacks()` |
| `setTraceHandler(fn)` | Receive line-level execution trace events (default) |
| `setOutputHandler(fn)` | Receive streaming stdout/stderr (default) |
| `SuspendCallback` | Throw from a handler to suspend execution |

## How it works

Eryx compiles CPython 3.14 to WebAssembly (WASI) using [componentize-py](https://github.com/bytecodealliance/componentize-py), then pre-initializes the interpreter so startup is fast. The JavaScript bindings are generated by [jco](https://github.com/bytecodealliance/jco) and use JSPI for async interop.

## Links

- [Documentation](https://docs.eryx.run)
- [GitHub](https://github.com/eryx-org/eryx)
- [Live demo](https://demo.eryx.run)

## License

MIT OR Apache-2.0
