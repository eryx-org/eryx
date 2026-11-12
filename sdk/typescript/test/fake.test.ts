/**
 * The client against a fake of the hosted API, as eryx.run's docs describe
 * it: one local server for HTTP and `/v1/stream`.
 */

import assert from "node:assert/strict";
import { mkdtemp, mkdir, symlink, writeFile } from "node:fs/promises";
import { type IncomingMessage, type ServerResponse, createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, test } from "node:test";

import { type WebSocket, WebSocketServer } from "ws";

import * as eryx from "../src/index.ts";
import { answer, mounts } from "../src/mounts.ts";

const KEY = "eryx_test_key";

const SUCCESS = {
  stdout: "4\n",
  stderr: "",
  compute_units: 1234,
  duration_ms: 12,
  peak_memory_bytes: 14286848,
  status: "success",
};

interface Request {
  method: string;
  path: string;
  headers: IncomingMessage["headers"];
  body: unknown;
}

type HttpHandler = (req: Request) => { status?: number; body?: unknown; headers?: Record<string, string> };
type StreamHandler = (ws: WebSocket, req: IncomingMessage) => void;

/** A fake eryx.run, recording the HTTP requests it gets */
async function fake(http: HttpHandler, stream?: StreamHandler) {
  const requests: Request[] = [];
  const wss = new WebSocketServer({ noServer: true, handleProtocols: (p) => (p.has("eryx.v1") ? "eryx.v1" : false) });
  const server = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    const request = { method: req.method!, path: req.url!, headers: req.headers, body: raw ? JSON.parse(raw) : undefined };
    requests.push(request);
    const { status = 200, body, headers = {} } = http(request);
    res.writeHead(status, { "Content-Type": "application/json", ...headers });
    res.end(body === undefined ? "" : JSON.stringify(body));
  });
  server.on("upgrade", (req, socket, head) => {
    if (!stream || req.headers.authorization !== `Bearer ${KEY}`) {
      socket.end("HTTP/1.1 401 Unauthorized\r\n\r\n");
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => stream(ws, req));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  after(() => {
    server.closeAllConnections();
    server.close();
  });
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  return { url, requests };
}

function messages(ws: WebSocket): AsyncIterator<Record<string, unknown>> {
  const queue: Record<string, unknown>[] = [];
  const waiting: ((m: Record<string, unknown>) => void)[] = [];
  ws.on("message", (data, isBinary) => {
    const message = isBinary ? { binary: data as Buffer } : JSON.parse(String(data));
    const next = waiting.shift();
    if (next) next(message);
    else queue.push(message);
  });
  return {
    next: () =>
      new Promise((resolve) => {
        const m = queue.shift();
        if (m) resolve({ value: m, done: false });
        else waiting.push((value) => resolve({ value, done: false }));
      }),
  };
}

async function recv(it: AsyncIterator<Record<string, unknown>>) {
  return (await it.next()).value as Record<string, any>;
}

describe("plain runs", () => {
  test("execute returns a result", async () => {
    const { url, requests } = await fake(() => ({ body: { ...SUCCESS, result: { a: 1 } } }));
    const result = await new eryx.Sandbox({ apiKey: KEY, baseUrl: url }).execute("print(2 + 2)");

    assert.equal(result.stdout, "4\n");
    assert.equal(result.fuelConsumed, 1234);
    assert.equal(result.durationMs, 12);
    assert.equal(result.peakMemoryBytes, 14286848);
    assert.deepEqual(result.result, { a: 1 });
    assert.equal(result.resultJson, '{"a":1}');
    assert.equal(result.stateWasReset, false);
    const [request] = requests;
    assert.equal(request!.path, "/v1/execute");
    assert.equal(request!.headers.authorization, `Bearer ${KEY}`);
    assert.deepEqual(request!.body, { code: "print(2 + 2)" });
  });

  test("limits and hosted options are sent", async () => {
    const { url, requests } = await fake(() => ({ body: SUCCESS }));
    await new eryx.Sandbox({
      apiKey: KEY,
      baseUrl: url,
      limits: { timeoutMs: 1500, maxFuel: 1e9 },
      environment: "templating",
      connections: ["github"],
      networkPolicies: [],
    }).execute("pass");
    assert.deepEqual(requests[0]!.body, {
      code: "pass",
      timeout_secs: 2,
      fuel_limit: 1e9,
      environment: "templating",
      connections: ["github"],
      network_policies: [],
    });
  });

  test("a missing key is explained", () => {
    const saved = process.env.ERYX_API_KEY;
    delete process.env.ERYX_API_KEY;
    try {
      assert.throws(() => new eryx.Sandbox(), /ERYX_API_KEY/);
    } finally {
      if (saved !== undefined) process.env.ERYX_API_KEY = saved;
    }
  });

  test("an empty ERYX_URL means the default", () => {
    const saved = process.env.ERYX_URL;
    process.env.ERYX_URL = "";
    try {
      const box = new eryx.Sandbox({ apiKey: KEY }) as unknown as { connection: { baseUrl: string } };
      assert.equal(box.connection.baseUrl, eryx.DEFAULT_URL);
    } finally {
      if (saved === undefined) delete process.env.ERYX_URL;
      else process.env.ERYX_URL = saved;
    }
  });

  test("an unreachable server is an APIConnectionError", async () => {
    for (const callbacks of [{}, { f: () => 1 }] as eryx.Callbacks[]) {
      const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: "http://127.0.0.1:9", callbacks });
      await assert.rejects(box.execute("pass"), eryx.APIConnectionError);
    }
  });

  test("aborting rejects with the signal's reason", async () => {
    const { url } = await fake(() => ({ body: SUCCESS }));
    const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: url });
    const reason = new Error("stop");
    await assert.rejects(box.execute("pass", { signal: AbortSignal.abort(reason) }), (e) => e === reason);
  });
});

describe("failed runs throw what the Python SDK raises", () => {
  const TRACEBACK = "Traceback (most recent call last):\n  ...\nValueError: bad input\n";
  const cases: [Record<string, unknown>, typeof eryx.EryxError][] = [
    [{ stdout: "loading\n", stderr: TRACEBACK, status: "error", error: TRACEBACK }, eryx.PythonExceptionError],
    [{ status: "timeout", error: "timed out" }, eryx.TimeoutError],
    [{ status: "resource_limit", error: "Fuel" }, eryx.ResourceLimitError],
    // A printed traceback isn't a raised exception
    [{ stderr: TRACEBACK, status: "error", error: "Failed to save session state" }, eryx.ExecutionError],
  ];
  for (const [response, Class] of cases) {
    test(Class.name, async () => {
      const { url } = await fake(() => ({ body: { compute_units: 99, duration_ms: 5, stdout: "", stderr: "", ...response } }));
      const error = await new eryx.Sandbox({ apiKey: KEY, baseUrl: url }).execute("...").catch((e: unknown) => e);
      assert.ok(error instanceof Class);
      assert.ok(error instanceof eryx.EryxError);
      assert.equal(error.constructor, Class);
      assert.equal(error.name, Class.name);
      assert.equal(error.fuelConsumed, 99);
      assert.equal(error.durationMs, 5);
      if (error instanceof eryx.PythonExceptionError) {
        assert.equal(error.stdout, "loading\n");
        assert.ok(error.stderr.endsWith("ValueError: bad input\n"));
        assert.equal(error.message, TRACEBACK);
      }
    });
  }
});

describe("error responses are typed", () => {
  const cases: [number, string, typeof eryx.APIError][] = [
    [401, "invalid_key", eryx.AuthenticationError],
    [404, "not_found", eryx.NotFoundError],
    [409, "session_busy", eryx.ConflictError],
    [429, "concurrency_limit", eryx.RateLimitError],
    [503, "unavailable", eryx.ServiceUnavailableError],
    [418, "something_new", eryx.APIError],
    [418, "toString", eryx.APIError],
  ];
  for (const [status, code, Class] of cases) {
    test(`${status} ${code}`, async () => {
      const { url } = await fake(() => ({ status, body: { error: { code, message: "nope" } }, headers: { "Retry-After": "7" } }));
      const error = await new eryx.Sandbox({ apiKey: KEY, baseUrl: url }).execute("pass").catch((e: unknown) => e);
      assert.equal((error as Error).constructor, Class);
      assert.ok(error instanceof eryx.APIError);
      assert.deepEqual([error.code, error.status, error.message], [code, status, "nope"]);
      if (error instanceof eryx.RateLimitError) assert.equal(error.retryAfter, 7);
    });
  }

  test("a body that isn't the API's", async () => {
    const { url } = await fake(() => ({ status: 502, body: "Bad gateway" }));
    const error = await new eryx.Sandbox({ apiKey: KEY, baseUrl: url }).execute("pass").catch((e: unknown) => e);
    assert.equal((error as Error).constructor, eryx.APIError);
    assert.equal((error as eryx.APIError).message, "HTTP 502");
  });
});

describe("sessions", () => {
  test("created on the first run, and deleted", async () => {
    const { url, requests } = await fake((req) => {
      if (req.path === "/v1/sessions") return { body: { id: "s1", environment: "python" } };
      if (req.method === "DELETE") return { status: 204 };
      return { body: SUCCESS };
    });
    const session = new eryx.Session({ apiKey: KEY, baseUrl: url, ttlSecs: 60 });
    assert.equal(session.id, undefined);
    await session.execute("x = 1");
    await session.execute("print(x)");
    assert.equal(session.id, "s1");
    assert.equal(session.executionCount, 2);
    await session.delete();
    assert.equal(session.id, undefined);
    assert.deepEqual(
      requests.map((r) => [r.method, r.path]),
      [
        ["POST", "/v1/sessions"],
        ["POST", "/v1/sessions/s1/execute"],
        ["POST", "/v1/sessions/s1/execute"],
        ["DELETE", "/v1/sessions/s1"],
      ],
    );
    assert.deepEqual(requests[0]!.body, { ttl_secs: 60 });
  });

  test("concurrent first runs create one session", async () => {
    const { url, requests } = await fake((req) => (req.path === "/v1/sessions" ? { body: { id: "s1" } } : { body: SUCCESS }));
    const session = new eryx.Session({ apiKey: KEY, baseUrl: url });
    await Promise.all([session.execute("a"), session.execute("b")]);
    assert.equal(requests.filter((r) => r.path === "/v1/sessions").length, 1);
  });

  test("one run's abort doesn't fail another waiting on the same creation", async () => {
    const { url } = await fake((req) => (req.path === "/v1/sessions" ? { body: { id: "s1" } } : { body: SUCCESS }));
    const session = new eryx.Session({ apiKey: KEY, baseUrl: url });
    const controller = new AbortController();
    const aborted = session.execute("a", { signal: controller.signal });
    const other = session.execute("b");
    controller.abort();
    await assert.rejects(aborted, { name: "AbortError" });
    assert.equal((await other).stdout, "4\n");
  });

  test("ids are escaped", async () => {
    const { url, requests } = await fake(() => ({ body: SUCCESS }));
    await new eryx.Session({ apiKey: KEY, baseUrl: url, id: "a/../b" }).execute("pass");
    assert.equal(requests[0]!.path, "/v1/sessions/a%2F..%2Fb/execute");
  });
});

describe("callbacks over /v1/stream", () => {
  /** Calls each listed callback once, concurrently, and completes with what they answered */
  const callEach: StreamHandler = async (ws, req) => {
    const it = messages(ws);
    const execute = await recv(it);
    ws.send(JSON.stringify({ type: "something_newer" }));
    execute.callbacks.forEach((cb: { name: string }, i: number) =>
      ws.send(JSON.stringify({ type: "callback", call_id: String(i), name: cb.name, args: { n: i } })),
    );
    const answers: Record<string, unknown> = {};
    for (const _ of execute.callbacks) {
      const reply = await recv(it);
      answers[reply.call_id] = "result" in reply ? reply.result : reply.error;
    }
    ws.send(
      JSON.stringify({
        type: "completed",
        ...SUCCESS,
        callback_invocations: Object.keys(answers).length,
        result: { answers, execute, protocol: ws.protocol, authorization: req.headers.authorization },
        trace_id: "abc",
      }),
    );
  };

  test("are answered", async () => {
    const { url } = await fake(() => ({}), callEach);
    const box = new eryx.Sandbox({
      apiKey: KEY,
      baseUrl: url,
      callbacks: {
        double: { description: "Doubles n", handler: async ({ n }) => n * 2 },
        sync: ({ n }) => `sync ${n}`,
        broken: () => {
          throw new RangeError("no such user");
        },
        nothing: () => {},
        bigint: () => 1n,
      },
    });
    const result = await box.execute("...");
    const { answers, execute, protocol, authorization } = result.result as Record<string, any>;

    assert.deepEqual(answers, {
      "0": 0,
      "1": "sync 1",
      "2": "RangeError: no such user",
      "3": null,
      // A result JSON can't hold is an error, so the code isn't left waiting
      "4": "TypeError: Do not know how to serialize a BigInt",
    });
    assert.equal(result.callbackInvocations, 5);
    assert.equal(result.traceId, "abc");
    assert.equal(protocol, "eryx.v1");
    assert.equal(authorization, `Bearer ${KEY}`);
    assert.deepEqual(execute.callbacks, [
      { name: "double", description: "Doubles n" },
      { name: "sync", description: "" },
      { name: "broken", description: "" },
      { name: "nothing", description: "" },
      { name: "bigint", description: "" },
    ]);
    assert.equal(execute.type, "execute");
    assert.equal(execute.mounts, undefined);
  });

  test("a session's runs send its id", async () => {
    const { url } = await fake(() => ({ body: { id: "s1" } }), callEach);
    const session = new eryx.Session({ apiKey: KEY, baseUrl: url, callbacks: { f: ({ n }) => n } });
    const result = await session.execute("x = await f(n=1)");
    assert.equal((result.result as Record<string, any>).execute.session_id, "s1");
  });

  test("stream errors are typed", async () => {
    const { url } = await fake(
      () => ({}),
      (ws) => ws.once("message", () => ws.send(JSON.stringify({ type: "error", code: "concurrency_limit", message: "Too many" }))),
    );
    const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: url, callbacks: { f: () => 1 } });
    const error = await box.execute("...").catch((e: unknown) => e);
    assert.ok(error instanceof eryx.RateLimitError);
    assert.equal(error.code, "concurrency_limit");
    assert.equal(error.status, undefined);
    assert.equal(error.message, "Too many");
  });

  test("a refused upgrade says why", async () => {
    const { url } = await fake(() => ({ status: 401, body: { error: { code: "invalid_key", message: "Invalid API key" } } }));
    const box = new eryx.Sandbox({ apiKey: "eryx_wrong", baseUrl: url, callbacks: { f: () => 1 } });
    await assert.rejects(box.execute("..."), eryx.AuthenticationError);
  });

  test("a dropped connection is an APIConnectionError", async () => {
    const { url } = await fake(
      () => ({}),
      (ws) => ws.once("message", () => ws.close(1011)),
    );
    const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: url, callbacks: { f: () => 1 } });
    await assert.rejects(box.execute("..."), eryx.APIConnectionError);
  });

  test("aborting closes the socket", async () => {
    let closed!: () => void;
    const wasClosed = new Promise<void>((resolve) => (closed = resolve));
    const { url } = await fake(
      () => ({}),
      (ws) => ws.on("close", closed),
    );
    const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: url, callbacks: { f: () => 1 } });
    const controller = new AbortController();
    const run = box.execute("...", { signal: controller.signal });
    setTimeout(() => controller.abort(), 50);
    await assert.rejects(run, { name: "AbortError" });
    await wasClosed;
  });
});

describe("volumes", () => {
  async function tree() {
    const root = await mkdtemp(join(tmpdir(), "eryx-run-"));
    const repo = join(root, "repo");
    await mkdir(join(repo, "src"), { recursive: true });
    await mkdir(join(repo, ".git"));
    await writeFile(join(repo, "README.md"), "hello\n");
    await writeFile(join(repo, "src", "big.bin"), Buffer.alloc(70 * 1024, 1));
    await writeFile(join(repo, ".env"), "SECRET=1\n");
    await writeFile(join(repo, "..data"), "kubernetes\n");
    await writeFile(join(repo, ".git", "config"), "[remote]\n");
    await writeFile(join(root, "outside.txt"), "outside\n");
    await symlink(join(root, "outside.txt"), join(repo, "escape"));
    await symlink(join(repo, ".env"), join(repo, "innocent"));
    return repo;
  }

  test("are mounted and served over the stream", async () => {
    const repo = await tree();
    const { url } = await fake(
      () => ({}),
      async (ws) => {
        const it = messages(ws);
        const execute = await recv(it);
        ws.send(JSON.stringify({ type: "fs", op_id: "1", op: "list", mount: "/mnt/repo", path: "" }));
        const listed = await recv(it);
        const inline = await recv(it);
        ws.send(JSON.stringify({ type: "fs", op_id: "2", op: "read", mount: "/mnt/repo", path: "src/big.bin", offset: 10, len: 5 }));
        const read = await recv(it);
        const data = await recv(it);
        ws.send(JSON.stringify({ type: "fs", op_id: "3", op: "stat", mount: "/mnt/repo", path: "../outside.txt" }));
        const refused = await recv(it);
        ws.send(
          JSON.stringify({
            type: "completed",
            ...SUCCESS,
            result: { mounts: execute.mounts, listed, inline: String(inline.binary), read, data: [...data.binary], refused },
          }),
        );
      },
    );
    const box = new eryx.Sandbox({ apiKey: KEY, baseUrl: url, volumes: [{ hostPath: repo, guestPath: "/mnt/repo" }] });
    const { mounts, listed, inline, read, data, refused } = (await box.execute("...")).result as Record<string, any>;
    assert.deepEqual(mounts, [{ path: "/mnt/repo" }]);
    assert.deepEqual(
      listed.entries.map((e: Record<string, unknown>) => [e.name, e.is_dir, e.inline ?? false]),
      // .env, and the links to outside and to .env, aren't listed
      [
        ["..data", false, true],
        [".git", true, false],
        ["README.md", false, true],
        ["src", true, false],
      ],
    );
    assert.equal(listed.data, 17);
    assert.equal(inline, "kubernetes\nhello\n");
    assert.equal(read.data, 5);
    assert.deepEqual(data, [1, 1, 1, 1, 1]);
    assert.equal(refused.error, "permission_denied");
  });

  test("refuse what they mustn't serve", async () => {
    const repo = await tree();
    const served = await mounts([{ hostPath: repo, guestPath: "/mnt/repo" }], eryx.DEFAULT_HIDDEN);
    const op = async (path: string, op = "stat") =>
      (await answer(served, { op_id: "x", op, mount: "/mnt/repo", path, offset: 0, len: 10 }))[0];

    assert.equal((await op("README.md")).error, undefined);
    assert.equal((await op("..data")).error, undefined);
    for (const path of [".env", ".ENV", ".git/config", "escape", "innocent", "missing"]) {
      assert.equal((await op(path)).error, "not_found", path);
    }
    for (const path of ["../outside.txt", "/etc/passwd", "src/../README.md", "./README.md", "src//x"]) {
      assert.equal((await op(path)).error, "permission_denied", path);
    }
    assert.equal((await op("src", "read")).error, "is_a_directory");
    assert.equal((await op("README.md", "list")).error, "not_a_directory");
    assert.equal((await op("README.md", "chmod")).error, "unsupported");
    assert.equal((await answer(served, { op_id: "x", op: "stat", mount: "/mnt/other", path: "" }))[0].error, "not_found");

    const everything = await mounts([{ hostPath: repo, guestPath: "/mnt/repo" }], []);
    assert.equal((await answer(everything, { op_id: "x", op: "stat", mount: "/mnt/repo", path: ".env" }))[0].error, undefined);
  });

  test("hidden patterns are fnmatch's", async () => {
    const repo = await tree();
    for (const name of ["secret1.txt", "secretx.txt", "a]b"]) await writeFile(join(repo, name), "");
    const served = await mounts([{ hostPath: repo, guestPath: "/mnt/repo" }], ["secret[0-9].txt", "secret[!0-9x]*", "a[]]b"]);
    const error = async (path: string) => (await answer(served, { op_id: "x", op: "stat", mount: "/mnt/repo", path }))[0].error;
    assert.equal(await error("secret1.txt"), "not_found");
    assert.equal(await error("secretx.txt"), undefined);
    assert.equal(await error("a]b"), "not_found");
    assert.equal(await error("README.md"), undefined);
  });

  test("guest paths are checked", () => {
    assert.throws(() => new eryx.Sandbox({ apiKey: KEY, volumes: [{ hostPath: ".", guestPath: "/data" }] }), /under \/mnt\//);
    const twice = [
      { hostPath: ".", guestPath: "/mnt/a" },
      { hostPath: "..", guestPath: "/mnt/a" },
    ];
    assert.throws(() => new eryx.Sandbox({ apiKey: KEY, volumes: twice }), /two volumes/);
  });
});
