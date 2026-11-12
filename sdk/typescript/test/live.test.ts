/**
 * The client against a real eryx.run.
 *
 * Skipped unless `ERYX_LIVE_TESTS=1`, with `ERYX_API_KEY` and `ERYX_URL` set
 * (e.g. a local eryx-run server and its seed key). Sessions need the server
 * to have a database.
 */

import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, test } from "node:test";

import * as eryx from "../src/index.ts";

describe("live", { skip: process.env.ERYX_LIVE_TESTS !== "1" && "set ERYX_LIVE_TESTS=1 to run" }, () => {
  test("execute", async () => {
    const result = await new eryx.Sandbox().execute("import math\nprint(math.factorial(5))\nresult = [1, 2]");
    assert.equal(result.stdout, "120\n");
    assert.deepEqual(result.result, [1, 2]);
    assert.ok(result.fuelConsumed! > 0);
    assert.ok(result.durationMs >= 0);
  });

  test("failures throw what the Python SDK raises", async () => {
    const error = await new eryx.Sandbox().execute("print('before')\nraise ValueError('bad input')").catch((e: unknown) => e);
    assert.ok(error instanceof eryx.PythonExceptionError);
    assert.equal(error.stdout, "before\n");
    assert.match(error.stderr, /ValueError: bad input/);

    const limited = new eryx.Sandbox({ limits: { maxFuel: 1_000_000 } });
    await assert.rejects(limited.execute("while True: pass"), eryx.ResourceLimitError);
  });

  test("a bad key", async () => {
    await assert.rejects(new eryx.Sandbox({ apiKey: "eryx_dev_wrong" }).execute("pass"), eryx.AuthenticationError);
    const streamed = new eryx.Sandbox({ apiKey: "eryx_dev_wrong", callbacks: { f: () => 1 } });
    await assert.rejects(streamed.execute("pass"), eryx.AuthenticationError);
  });

  test("callbacks", async () => {
    const calls: number[] = [];
    const sandbox = new eryx.Sandbox({
      callbacks: {
        get_user: {
          description: "Look up a user",
          handler: async ({ id }) => {
            calls.push(id);
            return { id, name: "Ada" };
          },
        },
        fails: () => {
          throw new RangeError("nope");
        },
      },
    });
    const result = await sandbox.execute(`
import asyncio
users = await asyncio.gather(get_user(id=1), get_user(id=2))
print(users[1]["name"])
try:
    await fails()
except RuntimeError as e:
    print(e)
`);
    assert.match(result.stdout, /^Ada\n.*RangeError: nope\n$/);
    assert.deepEqual(calls.sort(), [1, 2]);
    assert.equal(result.callbackInvocations, 3);
  });

  test("sessions", async () => {
    const session = new eryx.Session({ ttlSecs: 60 });
    try {
      await session.execute("x = 21");
      assert.equal((await session.execute("print(x * 2)")).stdout, "42\n");
      const streamed = new eryx.Session({ id: session.id!, callbacks: { add: ({ n }) => n + 1 } });
      assert.equal((await streamed.execute("print(await add(n=x))")).stdout, "22\n");
    } finally {
      await session.delete();
    }
  });

  test("volumes", async () => {
    const root = await mkdtemp(join(tmpdir(), "eryx-run-live-"));
    await writeFile(join(root, "hello.txt"), "hi from the host\n");
    await writeFile(join(root, ".env"), "SECRET=1\n");
    const sandbox = new eryx.Sandbox({ volumes: [{ hostPath: root, guestPath: "/mnt/host" }] });
    const result = await sandbox.execute(
      "import os\nprint(sorted(os.listdir('/mnt/host')))\nprint(open('/mnt/host/hello.txt').read(), end='')",
    );
    assert.equal(result.stdout, "['hello.txt']\nhi from the host\n");
  });
});
