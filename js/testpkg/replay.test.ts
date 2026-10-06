import { describe, expect, it, beforeEach } from "vitest";
import {
  execute,
  executeWithJournal,
  Sandbox,
  SuspendCallback,
  type CallbackJournal,
} from "@bsull/eryx";
import { setCallbackHandler, setCallbacks } from "@bsull/eryx/callbacks";

const CODE = `
a = await fetch(id=1)
b = await fetch(id=2)
print(a["v"] + b["v"])
`;

describe("callback replay", () => {
  let calls: string[];

  beforeEach(() => {
    calls = [];
    setCallbacks([
      { name: "fetch", description: "Fetch a value" },
      { name: "approve", description: "Await approval" },
    ]);
    setCallbackHandler((name, argsJson) => {
      calls.push(`${name} ${argsJson}`);
      const args = JSON.parse(argsJson);
      return JSON.stringify({ v: args.id * 10 });
    });
  });

  it("records a journal and replays it without calling the handler", async () => {
    const first = await new Sandbox().executeWithJournal(CODE);
    expect(first.error).toBeUndefined();
    expect(first.result?.stdout).toBe("30\n");
    expect(first.replayedCallbacks).toBe(0);
    expect(first.journal.code).toBe(CODE);
    expect(first.journal.entries).toMatchObject([
      {
        index: 0,
        name: "fetch",
        args_json: '{"id":1}',
        result: { Ok: { v: 10 } },
      },
      {
        index: 1,
        name: "fetch",
        args_json: '{"id":2}',
        result: { Ok: { v: 20 } },
      },
    ]);
    expect(calls).toHaveLength(2);

    // The journal is plain JSON, so it survives persistence.
    const journal: CallbackJournal = JSON.parse(JSON.stringify(first.journal));
    calls = [];
    const second = await executeWithJournal(CODE, { journal });
    expect(second.result?.stdout).toBe("30\n");
    expect(second.replayedCallbacks).toBe(2);
    expect(second.journal.entries).toEqual(first.journal.entries);
    expect(calls).toEqual([]);
  });

  it("matches on canonical args regardless of key order", async () => {
    const code = `print((await fetch(id=1, x=2))["v"])`;
    const first = await executeWithJournal(code);
    expect(first.journal.entries[0].args_json).toBe('{"id":1,"x":2}');
    calls = [];
    const second = await executeWithJournal(
      `print((await fetch(x=2, id=1))["v"])`,
      { journal: first.journal },
    );
    expect(second.replayedCallbacks).toBe(1);
    expect(calls).toEqual([]);
  });

  it("replays gather results independent of dispatch order", async () => {
    const code = `
import asyncio
xs = await asyncio.gather(fetch(id=1), fetch(id=2), fetch(id=3))
print([x["v"] for x in xs])
`;
    const first = await executeWithJournal(code);
    calls = [];
    const second = await executeWithJournal(code, { journal: first.journal });
    expect(second.result?.stdout).toBe("[10, 20, 30]\n");
    expect(second.replayedCallbacks).toBe(3);
    expect(calls).toEqual([]);
  });

  it("journals errors and replays them as Python exceptions", async () => {
    setCallbackHandler((name) => {
      calls.push(name);
      // A thrown non-Error value is delivered to Python as an exception.
      throw "not found";
    });
    const code = `
try:
    await fetch(id=1)
except Exception as e:
    print("caught", e)
`;
    const first = await executeWithJournal(code);
    expect(first.result?.stdout).toBe("caught not found\n");
    expect(first.journal.entries[0].result).toEqual({ Err: "not found" });

    calls = [];
    const second = await executeWithJournal(code, { journal: first.journal });
    expect(second.result?.stdout).toBe("caught not found\n");
    expect(second.replayedCallbacks).toBe(1);
    expect(calls).toEqual([]);
  });

  it("runs everything live after the first divergence", async () => {
    const first = await executeWithJournal(CODE);
    calls = [];
    // id=3 misses, so the later id=2 call runs live even though it is cached.
    const second = await executeWithJournal(
      `
a = await fetch(id=3)
b = await fetch(id=2)
print(a["v"] + b["v"])
`,
      { journal: first.journal },
    );
    expect(second.result?.stdout).toBe("50\n");
    expect(second.replayedCallbacks).toBe(0);
    expect(
      calls.map((c) => JSON.parse(c.split(" ").slice(1).join(" ")).id),
    ).toEqual([3, 2]);
  });

  it("suspends, then resumes from the recorded journal", async () => {
    let approved = false;
    setCallbackHandler((name, argsJson) => {
      calls.push(name);
      if (name === "approve" && !approved) {
        throw new SuspendCallback("awaiting approval");
      }
      return name === "approve" ? '"yes"' : JSON.stringify({ v: 10 });
    });
    const code = `
a = await fetch(id=1)
print("before")
ok = await approve(action="deploy")
print("after", ok)
`;
    const first = await executeWithJournal(code);
    expect(first.suspended).toEqual({
      name: "approve",
      argsJson: '{"action":"deploy"}',
      reason: "awaiting approval",
    });
    expect(first.error).toBeInstanceOf(SuspendCallback);
    // The suspended call is not journaled; only the prefix before it is.
    expect(first.journal.entries.map((e) => e.name)).toEqual(["fetch"]);
    expect(calls).toEqual(["fetch", "approve"]);

    approved = true;
    calls = [];
    const second = await executeWithJournal(code, { journal: first.journal });
    expect(second.suspended).toBeUndefined();
    expect(second.result?.stdout).toBe("before\nafter yes\n");
    expect(second.replayedCallbacks).toBe(1);
    expect(calls).toEqual(["approve"]);
  });

  it("halts the guest so nothing runs after a suspension", async () => {
    setCallbackHandler((name) => {
      calls.push(name);
      if (name === "approve") throw new SuspendCallback("later");
      return "{}";
    });
    const outcome = await executeWithJournal(`
try:
    await approve()
except BaseException:
    pass
await fetch(id=1)
`);
    expect(outcome.suspended?.reason).toBe("later");
    expect(calls).toEqual(["approve"]);

    // The sandbox remains usable afterwards.
    const after = await execute(`print("alive")`);
    expect(after.stdout).toBe("alive\n");
  });

  it("does not replay under plain execute", async () => {
    await executeWithJournal(CODE);
    calls = [];
    await execute(CODE);
    expect(calls).toHaveLength(2);
  });
  // Kept last: after a script raises, the guest event loop re-raises that
  // exception on the next callback-awaiting execution of the shared instance.
  it("returns the error and journal when the script fails", async () => {
    const outcome = await executeWithJournal(`
await fetch(id=1)
raise ValueError("boom")
`);
    expect(outcome.result).toBeUndefined();
    expect(outcome.error?.message).toContain("boom");
    expect(outcome.suspended).toBeUndefined();
    expect(outcome.journal.entries).toHaveLength(1);
  });
});
