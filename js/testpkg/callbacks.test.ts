import { describe, expect, it, beforeEach } from "vitest";
import { execute } from "@bsull/eryx";
import {
  getExecutionOptions,
  setCallbackHandler,
  setCallbacks,
  setTraceHandler,
} from "@bsull/eryx/callbacks";

describe("callbacks", () => {
  beforeEach(() => {
    setCallbackHandler(null);
    setCallbacks([]);
    setTraceHandler(null);
  });

  it("enables Python tracing only while a trace handler is registered", () => {
    expect(getExecutionOptions()).toEqual({
      pythonTracing: false,
      reuseEmptyCallbacks: false,
    });

    setTraceHandler(() => {});

    expect(getExecutionOptions()).toEqual({
      pythonTracing: true,
      reuseEmptyCallbacks: false,
    });
  });

  it("invokes a registered callback", async () => {
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
    expect(result.stdout).toBe("1234567890\n");
  });

  it("lists available callbacks", async () => {
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
