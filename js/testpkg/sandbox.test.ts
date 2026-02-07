import { describe, expect, it } from "vitest";
import { Sandbox } from "@bsull/eryx";

describe("Sandbox", () => {
  const sandbox = new Sandbox();

  it("executes simple print", async () => {
    const result = await sandbox.execute('print("hello")');
    expect(result.stdout).toBe("hello");
  });

  it("captures stdout from multiple prints", async () => {
    const result = await sandbox.execute(`
print("line 1")
print("line 2")
print("line 3")
`);
    expect(result.stdout).toBe("line 1\nline 2\nline 3");
  });

  it("handles arithmetic", async () => {
    const result = await sandbox.execute(`
x = 2 + 3
y = x * 4
print(f"{x}, {y}")
`);
    expect(result.stdout).toBe("5, 20");
  });

  it("handles data structures", async () => {
    const result = await sandbox.execute(`
lst = [1, 2, 3]
dct = {"a": 1, "b": 2}
print(f"list: {lst}")
print(f"dict: {dct}")
`);
    expect(result.stdout).toContain("list: [1, 2, 3]");
    expect(result.stdout).toContain("dict: {'a': 1, 'b': 2}");
  });

  it("returns stderr on errors", async () => {
    await expect(
      sandbox.execute('raise ValueError("test error")'),
    ).rejects.toThrow();
  });

  it("supports stdlib imports", async () => {
    const result = await sandbox.execute(`
import json
data = {"key": "value", "num": 42}
print(json.dumps(data, sort_keys=True))
`);
    expect(result.stdout).toBe('{"key": "value", "num": 42}');
  });

  it("imports non-pre-initialized stdlib modules", async () => {
    const result = await sandbox.execute(`
import pickle
data = {"hello": "world", "num": 42}
pickled = pickle.dumps(data)
restored = pickle.loads(pickled)
print(restored)
`);
    expect(result.stdout).toContain("hello");
    expect(result.stdout).toContain("world");
  });
});

describe("state persistence", () => {
  const sandbox = new Sandbox();

  it("persists variables across execute calls", async () => {
    await sandbox.execute("x = 42");
    const result = await sandbox.execute("print(x)");
    expect(result.stdout).toBe("42");
  });

  it("persists functions across execute calls", async () => {
    await sandbox.execute("def greet(name): return f'Hello, {name}!'");
    const result = await sandbox.execute("print(greet('World'))");
    expect(result.stdout).toBe("Hello, World!");
  });

  it("clears state", async () => {
    await sandbox.execute("y = 123");
    await sandbox.clearState();
    await expect(sandbox.execute("print(y)")).rejects.toThrow();
  });

  it("snapshots and restores state", async () => {
    const fresh = new Sandbox();
    await fresh.execute("counter = 10");

    // Snapshot with counter = 10
    const snapshot = new Uint8Array(await fresh.snapshotState());
    expect(snapshot.length).toBeGreaterThan(0);

    // Mutate state
    await fresh.execute("counter = 999");
    const mutated = await fresh.execute("print(counter)");
    expect(mutated.stdout).toBe("999");

    // Restore snapshot (counter should be 10 again)
    await fresh.restoreState(snapshot);
    const restored = await fresh.execute("print(counter)");
    expect(restored.stdout).toBe("10");
  });
});
