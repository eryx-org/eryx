import { describe, expect, it, vi } from "vitest";
import { PythonExceptionError, Sandbox } from "@eryx-org/eryx";

const callbacks = { add: ({ a, b }: Record<string, number>) => a + b };

describe("language: javascript", () => {
  it("runs code with callbacks, output and the result variable", async () => {
    using sandbox = new Sandbox({ language: "javascript", callbacks });
    const result = await sandbox.execute(`
console.log("sum", await add({ a: 2, b: 3 }));
result = { ok: true };
`);
    expect(result.stdout).toBe("sum 5\n");
    expect(result.result).toEqual({ ok: true });
  });

  it("keeps state across calls", async () => {
    using sandbox = new Sandbox({ language: "javascript" });
    await sandbox.execute("globalThis.x = 41");
    expect((await sandbox.execute("console.log(x + 1)")).stdout).toBe("42\n");
  });

  it("snapshots and restores state", async () => {
    using a = new Sandbox({ language: "javascript" });
    await a.execute("var count = 41; const pair = [1, 2]; function helper() {}");
    const snapshot = await a.snapshotState();
    expect(snapshot.skippedGlobals).toEqual(["helper"]);
    using b = new Sandbox({ language: "javascript" });
    await b.restoreState(snapshot);
    expect((await b.execute("console.log(count + 1, pair.length)")).stdout).toBe("42 2\n");
  });

  it.each([undefined, 30000])("rejects an uncaught exception (timeoutMs %s)", async (timeoutMs) => {
    using sandbox = new Sandbox({ language: "javascript", timeoutMs });
    const error = await sandbox
      .execute('console.log("before"); throw new Error("boom")')
      .catch((e) => e);
    expect(error).toBeInstanceOf(PythonExceptionError);
    expect(error.message).toMatch(/Error: boom/);
    expect(error.stdout).toBe("before\n");
    expect(error.exitCode).toBeUndefined();
  });
});

describe("language: shell", () => {
  it("runs a script with callbacks, output and the result variable", async () => {
    using sandbox = new Sandbox({ language: "shell", callbacks });
    const result = await sandbox.execute(`
echo "sum $(add --a 2 --b 3)"
echo oops >&2
result='{"ok": true}'
`);
    expect(result.stdout).toBe("sum 5\n");
    expect(result.stderr).toBe("oops\n");
    expect(result.result).toEqual({ ok: true });
  });

  it("streams output to the output handler", async () => {
    const chunks: string[] = [];
    using sandbox = new Sandbox({
      language: "shell",
      outputHandler: (stream, data) => chunks.push(`${stream}:${data}`),
    });
    await sandbox.execute("echo hi");
    expect(chunks.join("")).toBe("0:hi\n");
  });

  it.each([undefined, 30000])("rejects a nonzero exit status (timeoutMs %s)", async (timeoutMs) => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const error = await sandbox.execute("echo before; exit 3").catch((e) => e);
    expect(error).toBeInstanceOf(PythonExceptionError);
    expect(error.message).toBe("exit status 3");
    expect(error.exitCode).toBe(3);
    expect(error.stdout).toBe("before\n");
  });

  it("appends fd 2 output to a failure's message, like the Rust host", async () => {
    using sandbox = new Sandbox({ language: "shell" });
    const error = await sandbox.execute("echo oops >&2; exit 1").catch((e) => e);
    expect(error.message).toBe("exit status 1oops\n");
    expect(error.stderr).toBe("oops\n");
  });

  it("snapshots and restores state", async () => {
    using a = new Sandbox({ language: "shell" });
    await a.execute("count=41");
    using b = new Sandbox({ language: "shell" });
    await b.restoreState(await a.snapshotState());
    expect((await b.execute("echo $((count + 1))")).stdout).toBe("42\n");
  });
});

describe("language: python", () => {
  it("captures writes to fd 1 and 2, like the Rust host", async () => {
    using sandbox = new Sandbox();
    const result = await sandbox.execute(`
import os, sys
print("print")
os.write(1, b"fd1\\n")
sys.__stderr__.write("fd2\\n")
sys.__stderr__.flush()
`);
    expect(result.stdout).toBe("print\nfd1\n");
    expect(result.stderr).toBe("fd2\n");
  });

  it("keeps a timeoutMs sandbox's exception as a PythonExceptionError", async () => {
    using sandbox = new Sandbox({ timeoutMs: 30000 });
    const error = await sandbox.execute('print("a"); raise ValueError("x")').catch((e) => e);
    expect(error).toBeInstanceOf(PythonExceptionError);
    expect(error.stdout).toBe("a\n");
  });
});

it("rejects an unknown language", () => {
  // @ts-expect-error not a language
  expect(() => new Sandbox({ language: "ruby" })).toThrow(TypeError);
});

// A callback the guest would never install under its name is refused (#962).
describe("reserved callback names", () => {
  it("refuses JavaScript globals, including a dotted name's root", () => {
    for (const name of ["fetch", "crypto.subtle", "setTimeout", "URL.*"]) {
      expect(() => new Sandbox({ language: "javascript", callbacks: { [name]: () => 1 } })).toThrow(
        "collides with the JavaScript global",
      );
    }
    using ok = new Sandbox({ language: "javascript", callbacks: { fetch_user: () => 1 } });
    expect(ok).toBeDefined();
  });

  it("refuses shell builtins and the commands a callback would hide", () => {
    expect(() => new Sandbox({ language: "shell", callbacks: { echo: () => 1 } })).toThrow(
      "collides with the shell builtin or reserved word 'echo'",
    );
    expect(() => new Sandbox({ language: "shell", callbacks: { cat: () => 1 } })).toThrow(
      "collides with the command 'cat'",
    );
    using free = new Sandbox({ language: "shell", coreutils: false, callbacks: { cat: () => 1 } });
    expect(free).toBeDefined();
  });

  it("only warns for Python builtins", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    using sandbox = new Sandbox({ callbacks: { print: () => 1 } });
    expect(sandbox).toBeDefined();
    expect(warn).toHaveBeenCalledWith(expect.stringContaining("collides with the Python builtin 'print'"));
    warn.mockRestore();
  });
});
