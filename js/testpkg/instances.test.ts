import { describe, expect, it } from "vitest";
import { Sandbox, execute } from "@bsull/eryx";

describe("separate Sandbox instances", () => {
  it("don't share variables", async () => {
    const a = new Sandbox();
    const b = new Sandbox();
    await a.execute("x = 1");
    const result = await b.execute("print('x' in globals())");
    expect(result.stdout).toBe("False\n");
  });

  it("don't share patched builtins or modules", async () => {
    const a = new Sandbox();
    const b = new Sandbox();
    await a.execute(`
import builtins, json
builtins.len = lambda obj: -1
json.dumps = lambda obj, **kw: "patched"
`);
    const result = await b.execute("import json; print(len('ab'), json.dumps(1))");
    expect(result.stdout).toBe("2 1\n");
  });

  it("don't share the default sandbox's state", async () => {
    await execute("shared_name = 1");
    const result = await new Sandbox().execute("print('shared_name' in globals())");
    expect(result.stdout).toBe("False\n");
  });

  it("have their own filesystem", async () => {
    const a = new Sandbox();
    await a.execute(`
open("/site-packages/note.txt", "w").write("from a")
import os
path = "/python-stdlib/" + min(n for n in os.listdir("/python-stdlib") if n.endswith(".py"))
with open(path, "r+b") as f:
    f.write(b"# changed by a")
`);
    const b = new Sandbox();
    const result = await b.execute(`
import os
print(os.listdir("/site-packages"))
path = "/python-stdlib/" + min(n for n in os.listdir("/python-stdlib") if n.endswith(".py"))
print(open(path, "rb").read(14) == b"# changed by a")
`);
    expect(result.stdout).toBe("[]\nFalse\n");
  });

  it("use their own callback handlers, even when running at once", async () => {
    const make = (tag: string) =>
      new Sandbox({
        callbacks: [{ name: "whoami", description: "Name this sandbox" }],
        callbackHandler: async () => {
          await new Promise((resolve) => setTimeout(resolve, 10));
          return JSON.stringify(tag);
        },
      });
    const code = `
names = [c["name"] for c in list_callbacks()]
print(names, await whoami())
`;
    const [a, b] = await Promise.all([make("a").execute(code), make("b").execute(code)]);
    expect(a.stdout).toBe("['whoami'] a\n");
    expect(b.stdout).toBe("['whoami'] b\n");
  });

  it("stream output to their own handler", async () => {
    const chunks: string[] = [];
    const sandbox = new Sandbox({ outputHandler: (_stream, data) => chunks.push(data) });
    await new Sandbox().execute("print('other')");
    await sandbox.execute("print('mine')");
    expect(chunks.join("")).toBe("mine\n");
  });
});
