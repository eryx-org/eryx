import { describe, expect, it } from "vitest";
import { createServer } from "node:net";
import type { AddressInfo } from "node:net";
import { Sandbox, TimeoutError, setNetConfig } from "@bsull/eryx";

describe("timeoutMs", () => {
  it("rejects a call that runs too long, then starts fresh", async () => {
    const sandbox = new Sandbox({ timeoutMs: 1000 });
    await sandbox.execute("x = 1");
    const error = await sandbox.execute("while True: pass").catch((e) => e);
    expect(error).toBeInstanceOf(TimeoutError);
    const after = await sandbox.execute("print('x' in globals())");
    expect(after.stdout).toBe("False\n");
  });

  it("reports a timeout from executeWithJournal as the outcome's error", async () => {
    const sandbox = new Sandbox({ timeoutMs: 1000 });
    const outcome = await sandbox.executeWithJournal("while True: pass");
    expect(outcome.error).toBeInstanceOf(TimeoutError);
    expect(outcome.result).toBeUndefined();
  });

  it("keeps state, callbacks, output and snapshots working", async () => {
    const chunks: string[] = [];
    const sandbox = new Sandbox({
      timeoutMs: 10_000,
      callbacks: [{ name: "double", description: "Double a number" }],
      callbackHandler: async (_name, argsJson) => JSON.stringify(JSON.parse(argsJson).n * 2),
      outputHandler: (_stream, data) => chunks.push(data),
    });
    await sandbox.execute("n = 21");
    const result = await sandbox.execute("print(await double(n=n))\nresult = n");
    expect(result.stdout).toBe("42\n");
    expect(result.result).toBe(21);
    expect(chunks.join("")).toBe("42\n");

    const snapshot = await sandbox.snapshotState();
    await sandbox.clearState();
    await sandbox.restoreState(snapshot);
    expect((await sandbox.execute("print(n)")).stdout).toBe("21\n");

    const first = await sandbox.executeWithJournal("print(await double(n=1))");
    expect(first.error).toBeUndefined();
    const replay = await sandbox.executeWithJournal("print(await double(n=1))", {
      journal: first.journal,
    });
    expect(replay.replayedCallbacks).toBe(1);
  });

  it("applies this thread's network policy", async () => {
    const server = createServer((socket) => socket.pipe(socket));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const { port } = server.address() as AddressInfo;
    const sandbox = new Sandbox({ timeoutMs: 10_000 });
    const code = `
import socket
try:
    with socket.create_connection(("127.0.0.1", ${port})) as s:
        s.sendall(b"ping")
        print(s.recv(4))
except OSError as e:
    print(e)
`;
    try {
      expect((await sandbox.execute(code)).stdout).toContain("networking is disabled");
      setNetConfig({ allowedHosts: ["127.0.0.1"], blockedHosts: [], allowedNetworks: ["127.0.0.0/8"] });
      expect((await sandbox.execute(code)).stdout).toBe("b'ping'\n");
    } finally {
      setNetConfig(null);
      server.close();
    }
  });

  it("closes a timed-out sandbox's connections", async () => {
    let closed!: () => void;
    const socketClosed = new Promise<void>((resolve) => (closed = resolve));
    const server = createServer((socket) => socket.on("close", closed));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const { port } = server.address() as AddressInfo;
    setNetConfig({ allowedHosts: ["127.0.0.1"], blockedHosts: [], allowedNetworks: ["127.0.0.0/8"] });
    const sandbox = new Sandbox({ timeoutMs: 1000 });
    try {
      const error = await sandbox
        .execute(`
import socket
s = socket.create_connection(("127.0.0.1", ${port}))
while True: pass
`)
        .catch((e) => e);
      expect(error).toBeInstanceOf(TimeoutError);
      await socketClosed;
    } finally {
      setNetConfig(null);
      server.close();
    }
  });

  it("rejects an invalid value", () => {
    expect(() => new Sandbox({ timeoutMs: 0 })).toThrow(TypeError);
  });
});
