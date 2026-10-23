import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { createServer, type Server } from "node:net";
import { createServer as createHttpServer, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";
import { gzipSync } from "node:zlib";
import { Sandbox, setNetConfig } from "@bsull/eryx";
// The browser transport, exercised directly: Node has fetch() but no CORS.
// @ts-expect-error -- untyped internal module
import { tcp as fetchTcp } from "../eryx/shims/net-fetch.js";

const sandbox = new Sandbox();
// Tests talk to 127.0.0.1, which the default blocklists refuse.
const allowLoopback = {
  allowedHosts: ["127.0.0.1"],
  blockedHosts: [],
  allowedNetworks: ["127.0.0.0/8"],
};

describe("network policy", () => {
  afterEach(() => setNetConfig(null));

  const tryConnect = (host: string) =>
    sandbox.execute(`
import socket
try:
    socket.create_connection(("${host}", 9))
except OSError as e:
    print(e)
`);

  it("is disabled by default", async () => {
    expect((await tryConnect("example.com")).stdout).toContain("networking is disabled");
  });

  it("refuses hosts outside the allowlist", async () => {
    setNetConfig({ allowedHosts: ["*.example.com"] });
    expect((await tryConnect("example.org")).stdout).toContain("not in allowed list");
  });

  it("blocks private hosts even with allowAllHosts", async () => {
    setNetConfig({ allowAllHosts: true });
    expect((await tryConnect("127.0.0.1")).stdout).toContain("is blocked");
  });

  it("blocks private addresses that pass the host patterns", async () => {
    setNetConfig({ allowAllHosts: true, blockedHosts: [] });
    expect((await tryConnect("127.0.0.1")).stdout).toContain("resolves to blocked address 127.0.0.1");
    // A name is checked by what it resolves to, not how it is spelled.
    expect((await tryConnect("localhost")).stdout).toContain("resolves to blocked address");
  });
});

describe("Node socket transport", () => {
  let server: Server;
  let port: number;

  beforeAll(async () => {
    setNetConfig(allowLoopback);
    // Upper-cases what it receives after a delay, so the host-side read
    // genuinely has to suspend.
    server = createServer((sock) => {
      sock.on("data", (d) => setTimeout(() => sock.end(d.toString().toUpperCase()), 50));
    });
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    port = (server.address() as AddressInfo).port;
  });

  afterAll(() => {
    setNetConfig(null);
    server.close();
  });

  it("lets internalHosts resolve to blocked networks", async () => {
    setNetConfig({ allowAllHosts: true, blockedHosts: [], internalHosts: ["localhost"] });
    try {
      const result = await sandbox.execute(`
import socket
s = socket.create_connection(("localhost", ${port}))
s.sendall(b"internal")
print(s.recv(100).decode())
s.close()
`);
      expect(result.stdout).toBe("INTERNAL\n");
    } finally {
      setNetConfig(allowLoopback);
    }
  });

  it("round-trips over a real TCP socket", async () => {
    const result = await sandbox.execute(`
import socket
s = socket.create_connection(("127.0.0.1", ${port}))
s.sendall(b"hello eryx")
print(s.recv(100).decode())
s.close()
`);
    expect(result.stdout).toBe("HELLO ERYX\n");
  });

  it("surfaces connection-refused", async () => {
    const result = await sandbox.execute(`
import socket
try:
    socket.create_connection(("127.0.0.1", 1))
except OSError as e:
    print("refused:", e)
`);
    expect(result.stdout).toContain("refused");
  });
});

describe("fetch transport", () => {
  let server: HttpServer;
  let port: number;

  beforeAll(async () => {
    server = createHttpServer((req, res) => {
      let body = "";
      req.on("data", (d) => (body += d));
      req.on("end", () => {
        const payload = JSON.stringify({ method: req.method, url: req.url, body, ua: req.headers["x-test"] });
        res.setHeader("content-type", "application/json");
        res.setHeader("content-encoding", "gzip");
        res.end(gzipSync(payload));
      });
    });
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    port = (server.address() as AddressInfo).port;
  });

  afterAll(() => server.close());

  const exchange = async (request: string) => {
    const h = await fetchTcp.connect("127.0.0.1", port, 0);
    await fetchTcp.write(h, 0, new TextEncoder().encode(request));
    const chunks: Uint8Array[] = [];
    for (let c; (c = await fetchTcp.read(h, 7, 0)).length; ) chunks.push(c);
    fetchTcp.close(h);
    return Buffer.concat(chunks).toString();
  };

  it("replays a POST and re-frames the decoded response", async () => {
    const raw = await exchange(
      "POST /echo?x=1 HTTP/1.1\r\nHost: ignored\r\nX-Test: yes\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nhello",
    );
    const [head, body] = raw.split("\r\n\r\n");
    expect(head).toMatch(/^HTTP\/1\.1 200 OK\r\n/);
    expect(head).not.toMatch(/content-encoding/i);
    expect(head).toContain(`content-length: ${Buffer.byteLength(body)}`);
    expect(head).toContain("connection: close");
    expect(JSON.parse(body)).toEqual({ method: "POST", url: "/echo?x=1", body: "hello", ua: "yes" });
  });

  it("rejects non-HTTP traffic", async () => {
    await expect(exchange("PING\r\n")).rejects.toEqual({
      payload: { tag: "not-permitted", val: "only HTTP(S) requests are supported in the browser" },
    });
  });
});

describe("Python HTTP clients over eryx:net", () => {
  let server: HttpServer;
  let port: number;
  let requestedPath: string | undefined;

  beforeAll(async () => {
    setNetConfig(allowLoopback);
    server = createHttpServer((req, res) => {
      requestedPath = req.url;
      res.end("hello from the host");
    });
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    port = (server.address() as AddressInfo).port;
  });

  afterAll(() => {
    setNetConfig(null);
    server.close();
  });

  it("urllib.request works", async () => {
    const result = await sandbox.execute(`
import urllib.request
print(urllib.request.urlopen("http://127.0.0.1:${port}/hi").read().decode())
`);
    expect(result.stdout).toBe("hello from the host\n");
    expect(requestedPath).toBe("/hi");
  });
});
