import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { COREUTILS_TOOLS, PythonExceptionError, Sandbox } from "@eryx-org/eryx";

const failingUrl = new URL("./fixtures/failing-command.js", import.meta.url).href;

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

  it("runs sleep for as long as it's asked", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const start = performance.now();
    const result = await sandbox.execute('sleep 1; echo "rc=$?"');
    expect(result.stdout).toBe("rc=0\n");
    expect(performance.now() - start).toBeGreaterThan(900);
  });

  it("refuses more than 16 MiB of stdin", async () => {
    using sandbox = new Sandbox({ language: "shell", timeoutMs });
    const result = await sandbox.execute(
      // About 20 MiB, appended by the shell from three children's output.
      'mkdir /tmp; for i in 1 2 3; do seq 1 1000000 >> /tmp/a; done; wc -c < /tmp/a; echo "wc=$?"',
    );
    expect(result.stdout).toContain("wc=134");
    expect(result.stderr).toContain("wc: limit-exceeded");
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

  it("gives exit status 1 to a command whose run returns its error", async () => {
    using sandbox = new Sandbox({ language: "shell", commands: { fails: failingUrl } });
    const result = await sandbox.execute('fails; echo "fails=$?"');
    expect(result.stdout).toBe("fails=1\n");
  });

  it("reports a command that can't load as crashed, and goes on", async () => {
    using sandbox = new Sandbox({
      language: "shell",
      commands: { gone: new URL("./fixtures/missing.js", import.meta.url).href },
    });
    const result = await sandbox.execute('gone; echo "gone=$?"; echo still | cat');
    expect(result.stdout).toBe("gone=134\nstill\n");
    expect(result.stderr).toContain("gone: spawn-failed");
  });

  it("checks its options", () => {
    expect(() => new Sandbox({ commands: { cat: coreutilsUrl } })).toThrow(/only for language "shell"/);
    expect(() => new Sandbox({ language: "shell", commands: { cat: "./cat.js" } })).toThrow(
      /not an absolute URL/,
    );
    expect(COREUTILS_TOOLS).toContain("sort");
  });
});

describe("shell command budgets", () => {
  // The host directly: what a leftover child holds doesn't carry into the
  // next execution.
  it("are per execution", async () => {
    const { createProcessHost } = await import("../eryx/shims/process.js");
    const host = createProcessHost(
      { dir: {} },
      new Map([["cat", async () => ({ instantiate: async () => ({}), getCoreModule: () => new WebAssembly.Module(new Uint8Array()) })]]),
    );
    const spawn = () => host.Child.spawn("cat", [], [], "/", undefined);
    // 64 children spawned and never waited for (the call was cancelled).
    for (let i = 0; i < 64; i++) spawn();
    expect(spawn).toThrow(expect.objectContaining({ payload: "limit-exceeded" }));
    host.startExecution();
    expect(() => spawn()).not.toThrow();
  });
});
