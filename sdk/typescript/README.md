# @eryx-org/eryx-run

Run untrusted Python in an [eryx](https://github.com/eryx-org/eryx) sandbox on
[eryx.run](https://eryx.run), from TypeScript or JavaScript. The sandboxed code
can `await` functions you provide, and read directories you serve, both from
your process.

```sh
npm install @eryx-org/eryx-run
export ERYX_API_KEY=eryx_...   # from https://app.eryx.run
```

```ts
import { Sandbox } from "@eryx-org/eryx-run";

const sandbox = new Sandbox();
const result = await sandbox.execute("print(2 + 2)");
console.log(result.stdout); // "4\n"
```

It has no dependencies. It needs `fetch`, and, for callbacks and volumes, a
`WebSocket` that can send an `Authorization` header: Node.js 22+ and Bun have
one. Elsewhere, pass `WebSocket` (e.g. from the [`ws`](https://www.npmjs.com/package/ws)
package). Browsers can't set the header, and shouldn't hold your API key
anyway, so run it on your server.

## Callbacks

Sandboxed code can `await` functions you provide, called with its keyword
arguments. They can be async, and must return JSON:

```ts
const sandbox = new Sandbox({
  callbacks: {
    get_user: {
      description: "Look up a user by id",
      handler: async ({ id }) => ({ id, name: "Ada" }),
    },
    now: () => Date.now(), // no description
  },
});

await sandbox.execute(`
user = await get_user(id=123)
print(user["name"])
`);
```

If a callback throws, the code gets a `RuntimeError` with its message.

## Results and errors

`execute` resolves to an `ExecuteResult`: `stdout`, `stderr`, `durationMs`,
`fuelConsumed` (compute units), `callbackInvocations`, `peakMemoryBytes`,
`networkBytes`, `traceId`, and `result`, the code's `result` variable as JSON:

```ts
(await sandbox.execute("result = {'answer': 42}")).result; // { answer: 42 }
```

A run that fails rejects with the error the Python SDK raises:
`PythonExceptionError` (with `stdout` and `stderr`) when the code raises,
`TimeoutError`, or `ResourceLimitError`. Requests eryx.run refuses reject with
an `APIError` subclass: `AuthenticationError`, `RateLimitError` (with
`retryAfter`), `NotFoundError`, `ConflictError`, `InvalidRequestError` or
`ServiceUnavailableError`; `APIConnectionError` if eryx.run can't be reached
or the connection drops mid-run. All of them are `EryxError`s, and say what
the run used (`fuelConsumed`, `durationMs`, ...) when it ran.

```ts
import { PythonExceptionError } from "@eryx-org/eryx-run";

try {
  await sandbox.execute("raise ValueError('bad input')");
} catch (e) {
  if (e instanceof PythonExceptionError) console.log(e.stderr); // ends with the traceback
}
```

Pass a `signal` to stop waiting for a run:
`sandbox.execute(code, { signal: AbortSignal.timeout(10_000) })`. The run
itself carries on on eryx.run until it finishes or hits a limit, so set
`limits` to bound it.

## Options

```ts
new Sandbox({
  limits: { timeoutMs: 5_000, maxFuel: 1_000_000_000 }, // default: your org's limits
  environment: "templating",   // an environment you uploaded
  connections: ["github"],     // MCP connections, as mcp.github.<tool>()
  networkPolicies: [],         // no network (default: your org's default policies)
  apiKey: "eryx_...",          // default: $ERYX_API_KEY
  baseUrl: "https://api.eryx.run", // default: $ERYX_URL, else this
});
```

## Sessions

A `Session` keeps variables, imports and files under `/data` between runs:

```ts
import { Session } from "@eryx-org/eryx-run";

const session = new Session({ ttlSecs: 600 });
await session.execute("x = 21");
(await session.execute("print(x * 2)")).stdout; // "42\n"
await session.delete();
```

It's created on the first run. `new Session({ id })` uses an existing one,
such as one another process created (`session.id`). Runs in a session go one
at a time.

## Volumes

`volumes` mounts local directories, read-only, at paths under `/mnt/`.
They're served from your process as the code reads them, so nothing is
uploaded first:

```ts
const sandbox = new Sandbox({ volumes: [{ hostPath: "./my-repo", guestPath: "/mnt/repo" }] });
await sandbox.execute("import os; print(os.listdir('/mnt/repo'))");
```

The code can ask for any path, so it never gets `..`, symlinks that lead out
of the directory, or files matching `hidden` (default `DEFAULT_HIDDEN`:
`.env`, keys, `.git/config` and the like, which look missing). Pass
`hidden: []` to serve everything, or your own patterns. Volumes need
`node:fs` (Node.js, Bun or Deno).

## Examples

[`examples/`](examples) has runnable scripts: callbacks an agent's code
calls, a session used like a notebook, and a volume over a local directory.

```sh
npm install && npm run build
ERYX_API_KEY=eryx_... node examples/callbacks.ts
```

## Development

```sh
npm install
npm run build                             # the examples type-check against dist/
npm run typecheck
npm test                                  # against a fake eryx.run
ERYX_LIVE_TESTS=1 npm test                # also against $ERYX_URL with $ERYX_API_KEY
```

See the [API reference](https://eryx.run/docs) for the limits and errors.
