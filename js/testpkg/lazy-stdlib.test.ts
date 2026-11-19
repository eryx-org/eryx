import { expect, it } from "vitest";
import { Sandbox, _fileTree } from "@eryx-org/eryx";

// Its own file, so its own module instance: nothing has loaded the stdlib yet.
it("loads the Python stdlib only for the first Python sandbox", async () => {
  expect(_fileTree.dir["python-stdlib"]).toBeUndefined();

  using js = new Sandbox({ language: "javascript" });
  expect((await js.execute("console.log(1 + 1)")).stdout).toBe("2\n");
  using shell = new Sandbox({ language: "shell" });
  expect((await shell.execute("echo hi")).stdout).toBe("hi\n");
  expect(_fileTree.dir["python-stdlib"]).toBeUndefined();

  using python = new Sandbox();
  expect((await python.execute("import json; print(json.dumps([1]))")).stdout).toBe("[1]\n");
  expect(_fileTree.dir["python-stdlib"]).toBeDefined();

  // Later sandboxes reuse it; one in a Worker loads its own there.
  const later = [new Sandbox(), new Sandbox({ timeoutMs: 30000 })];
  for (const sandbox of later) {
    expect((await sandbox.execute("import textwrap; print('ok')")).stdout).toBe("ok\n");
    sandbox.close();
  }
});
