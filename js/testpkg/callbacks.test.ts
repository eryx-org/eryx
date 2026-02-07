import { describe, expect, it, beforeEach } from "vitest";
import { execute } from "@bsull/eryx";
import { setCallbackHandler, setCallbacks } from "@bsull/eryx/callbacks";

describe("callbacks", () => {
  beforeEach(() => {
    setCallbackHandler(null);
    setCallbacks([]);
  });

  // Skipped: jco 1.16.1 has bugs in async import lowering that prevent callbacks from working.
  // The issues are: (1) moduleIdx is null for all lowered imports so GlobalComponentAsyncLowers
  // lookup fails, (2) _lowerImport's deferred call lacks WebAssembly.promising wrapper,
  // (3) driver loop result unpacking expects a number but gets an object.
  // See ~/jco-1.16.1-bugs.md for details.
  it.skip("invokes a registered callback", async () => {
    setCallbackHandler((name, argsJson) => {
      if (name === "get_time") {
        return JSON.stringify({ timestamp: 1234567890 });
      }
      throw new Error(`Unknown callback: ${name}`);
    });

    setCallbacks([
      { name: "get_time", description: "Returns current timestamp" },
    ]);

    const result = await execute(`
result = await get_time()
print(result["timestamp"])
`);
    expect(result.stdout).toBe("1234567890");
  });

  it.skip("lists available callbacks", async () => {
    setCallbacks([
      { name: "alpha", description: "First callback" },
      { name: "beta", description: "Second callback" },
    ]);

    const result = await execute(`
callbacks = list_callbacks()
for cb in callbacks:
    print(f"{cb['name']}: {cb['description']}")
`);
    expect(result.stdout).toContain("alpha: First callback");
    expect(result.stdout).toContain("beta: Second callback");
  });

  it("returns error when no handler is set", async () => {
    await expect(
      execute(`
result = await invoke("missing", "{}")
`),
    ).rejects.toThrow();
  });
});
