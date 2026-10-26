import { describe, expect, it } from "vitest";
import { createServer } from "node:net";
import type { AddressInfo } from "node:net";
import { Sandbox, execute, setNetConfig } from "@bsull/eryx";

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

  it("can't use each other's connection handles", async () => {
    const received: string[] = [];
    const server = createServer((socket) => socket.on("data", (d) => received.push(d.toString())));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const { port } = server.address() as AddressInfo;
    setNetConfig({ allowedHosts: ["127.0.0.1"], blockedHosts: [], allowedNetworks: ["127.0.0.0/8"] });

    // `a` holds a connection open while it waits on its callback.
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    let opened!: () => void;
    const isOpen = new Promise<void>((resolve) => (opened = resolve));
    const a = new Sandbox({
      callbacks: [{ name: "hold", description: "Wait for the test" }],
      callbackHandler: async () => {
        opened();
        await held;
        return "null";
      },
    });
    try {
      const running = a.execute(`
import _eryx
handle = _eryx._eryx_tcp_connect("127.0.0.1", ${port})
await hold()
`);
      await isOpen;
      const b = await new Sandbox().execute(`
import _eryx
for handle in range(64):
    try:
        _eryx._eryx_tcp_write(handle, b"from b")
        print("wrote", handle)
    except OSError:
        pass
`);
      release();
      await running;
      expect(b.stdout).toBe("");
      expect(received).toEqual([]);
    } finally {
      release();
      setNetConfig(null);
      server.close();
    }
  });
});
