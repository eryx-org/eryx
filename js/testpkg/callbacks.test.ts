import { describe, expect, it, beforeEach } from "vitest";
import * as shared from "@eryx-org/eryx-run/shared";
import {
  execute,
  PythonExceptionError,
  Sandbox,
  TimeoutError,
} from "@eryx-org/eryx";
import {
  getExecutionOptions,
  setOutputHandler,
  setTraceHandler,
} from "@eryx-org/eryx/callbacks";

describe("callbacks", () => {
  beforeEach(() => {
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

  it("calls a callback with the code's keyword arguments", async () => {
    const sandbox = new Sandbox({
      callbacks: {
        get_user: async ({ id }) => ({ id, name: "Ada" }),
        nothing: () => {},
      },
    });
    const result = await sandbox.execute(`
user = await get_user(id=123)
print(user["name"], user["id"], await nothing())
`);
    expect(result.stdout).toBe("Ada 123 None\n");
    expect(result.callbackInvocations).toBe(2);
  });

  it("lists callbacks with their descriptions", async () => {
    const sandbox = new Sandbox({
      callbacks: {
        alpha: { description: "First callback", handler: () => 1 },
        beta: () => 2,
      },
    });
    const result = await sandbox.execute(`
for cb in list_callbacks():
    print(f"{cb['name']}: {cb['description']}")
`);
    expect(result.stdout).toBe("alpha: First callback\nbeta: \n");
  });

  it("raises in Python when a callback throws, or returns what JSON can't hold", async () => {
    const sandbox = new Sandbox({
      callbacks: {
        broken: () => {
          throw new RangeError("no such user");
        },
        cyclic: () => {
          const value: Record<string, unknown> = {};
          value.self = value;
          return value;
        },
        big: () => 12345678901234567890n,
      },
    });
    const result = await sandbox.execute(`
for name in ["broken", "cyclic"]:
    try:
        await globals()[name]()
    except Exception as e:
        print(name, "raised")
print(await big())
`);
    expect(result.stdout).toBe("broken raised\ncyclic raised\n12345678901234567890\n");
  });

  it("refuses the callbacks API it replaced, saying what to use", () => {
    expect(
      () => new Sandbox({ callbacks: [{ name: "f", description: "" }] as never }),
    ).toThrow(/object of functions by name/);
    expect(
      () => new Sandbox({ callbackHandler: () => "null" } as never),
    ).toThrow(/callbackHandler is gone/);
    expect(() => new Sandbox({ callbacks: { f: "nope" } as never })).toThrow(
      /isn't a function/,
    );
  });

  it("has no callbacks outside a Sandbox's own", async () => {
    await expect(execute(`await invoke("missing", "{}")`)).rejects.toThrow();
  });

  it("does not carry a partial UTF-8 sequence into the next execution", async () => {
    const chunks: string[] = [];
    setOutputHandler((stream, data) => {
      if (stream === 0) chunks.push(data);
    });
    try {
      // Lone halves of "é" (0xC3 0xA9), written via surrogateescape.
      await execute(`print("\\udcc3", end="")`);
      const first = chunks.join("");
      chunks.length = 0;
      await execute(`print("\\udca9", end="")`);
      expect([first, chunks.join("")]).toEqual(["�", "�"]);
    } finally {
      setOutputHandler(null);
    }
  });
});

describe("shared with eryx.run's client", () => {
  it("throws its error classes, so one instanceof handles both", async () => {
    expect(PythonExceptionError).toBe(shared.PythonExceptionError);
    expect(TimeoutError).toBe(shared.TimeoutError);

    const error = await new Sandbox()
      .execute(`print("before")\nraise ValueError("bad input")`)
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(shared.PythonExceptionError);
    expect(error).toBeInstanceOf(shared.EryxError);
    const raised = error as shared.PythonExceptionError;
    expect(raised.name).toBe("PythonExceptionError");
    expect(raised.message).toMatch(/^Traceback/);
    expect(raised.message).toContain("ValueError: bad input");
    expect(raised.stdout).toBe("before\n");
    expect(raised.stderr).toContain("ValueError: bad input");
    expect(raised.durationMs).toBeGreaterThanOrEqual(0);
  });

  it("returns its ExecuteResult, with the stats a local run has", async () => {
    const result: shared.ExecuteResult = await new Sandbox().execute("result = 1");
    expect(result.result).toBe(1);
    expect(result.durationMs).toBeGreaterThanOrEqual(0);
    expect(result.callbackInvocations).toBe(0);
    expect(result.unsavedVariables).toEqual([]);
    expect(result.fuelConsumed).toBeUndefined();
  });
});
