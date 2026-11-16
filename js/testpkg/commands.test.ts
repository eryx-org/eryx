import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { COREUTILS_TOOLS, PythonExceptionError, Sandbox } from "@eryx-org/eryx";

// The bundled coreutils' jco output, registered again under another name.
const coreutilsUrl = pathToFileURL(
  createRequire(import.meta.url).resolve("@eryx-org/eryx/coreutils"),
).href;

// The shell's external commands (shims/process.js), with and without a Worker.
describe.each([undefined, 30000])("shell commands (timeoutMs %s)", (timeoutMs) => {
  it("runs coreutils in pipelines", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const result = await sandbox.execute("printf 'pear\\napple\\n' | sort | head -n 1");
    expect(result.stdout).toBe("apple\n");
  });

  it("shares the sandbox's filesystem both ways", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const result = await sandbox.execute(
      "mkdir -p /tmp && echo hi > /tmp/a; cat /tmp/a; echo there | tee /tmp/b > /tmp/c; read l < /tmp/b; echo \"read=$l\"",
    );
    expect(result.stdout).toBe("hi\nread=there\n");
  });

  it("passes stdin, exit codes and stderr through", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const result = await sandbox.execute(
      'wc -l <<EOF\none\ntwo\nEOF\ncat /missing; echo "cat=$?"; nope; echo "nope=$?"',
    );
    expect(result.stdout).toBe("2\ncat=1\nnope=127\n");
    expect(result.stderr).toContain("/missing");
    expect(result.stderr).toContain("command not found: nope");
  });

  it("caps a command's output at 16 MiB and fails it", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    // About 20 MiB of numbers.
    const result = await sandbox.execute(
      'mkdir /tmp; seq 1 3000000 > /tmp/n; echo "seq=$?"; wc -c < /tmp/n',
    );
    const [status, size] = result.stdout.trim().split("\n");
    expect(status).not.toBe("seq=0");
    // The write that would cross the cap is refused whole.
    expect(Number(size)).toBeLessThanOrEqual(16 * 1024 * 1024);
    expect(Number(size)).toBeGreaterThan(15 * 1024 * 1024);
  }, 60_000);
});

describe("shell command options", () => {
  it("registers more commands by URL", async () => {
    using sandbox = new Sandbox({ language: "shell", coreutils: false, commands: { cat: coreutilsUrl } });
    const result = await sandbox.execute('echo b | cat; echo a | sort; echo "sort=$?"');
    expect(result.stdout).toBe("b\nsort=127\n");
  });

  it("leaves coreutils out with coreutils: false", async () => {
    using sandbox = new Sandbox({ language: "shell", coreutils: false });
    const error = await sandbox.execute("echo a | sort").catch((e) => e);
    expect(error).toBeInstanceOf(PythonExceptionError);
    expect(error.exitCode).toBe(127);
  });

  it("checks its options", () => {
    expect(() => new Sandbox({ commands: { cat: coreutilsUrl } })).toThrow(/only for language "shell"/);
    expect(() => new Sandbox({ language: "shell", commands: { cat: "./cat.js" } })).toThrow(
      /not an absolute URL/,
    );
    expect(COREUTILS_TOOLS).toContain("sort");
  });
});
