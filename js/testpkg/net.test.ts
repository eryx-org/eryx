import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { createServer, type Server, type Socket } from "node:net";
import { createServer as createHttpServer, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";
import { gzipSync } from "node:zlib";
import { Sandbox, setNetConfig } from "@bsull/eryx";
// The browser transport, exercised directly: Node has fetch() but no CORS.
// @ts-expect-error -- untyped internal module
import { tcp as fetchTcp, networkFilter as fetchNetworkFilter } from "../eryx/shims/net-fetch.js";
// @ts-expect-error -- untyped internal module
import { tcp as nodeTcp } from "../eryx/shims/net-node.js";
import { DEFAULT_BLOCKED_NETWORKS } from "@bsull/eryx";

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
    expect((await tryConnect("127.0.0.1")).stdout).toContain("resolves to a blocked network");
    // A name is checked by what it resolves to, not how it is spelled.
    expect((await tryConnect("localhost")).stdout).toContain("resolves to a blocked network");
    // NAT64 addresses are checked by the IPv4 address they embed.
    expect((await tryConnect("64:ff9b::a9fe:a9fe")).stdout).toContain("resolves to a blocked network");
  });

  it("matches hosts in their URL form", async () => {
    setNetConfig({ allowAllHosts: true });
    // Both spell 127.0.0.1, which `127.*` blocks.
    expect((await tryConnect("2130706433")).stdout).toContain("is blocked");
    expect((await tryConnect("0x7f.1")).stdout).toContain("is blocked");
    expect((await tryConnect("0.0.0.0")).stdout).toContain("is blocked");
    expect((await tryConnect("::")).stdout).toContain("is blocked");
    expect((await tryConnect("evil.com@example.com")).stdout).toContain("not a valid host name");
  });

  it("rejects malformed CIDRs up front", () => {
    // `Number("")` is 0: unchecked, this would exempt every IPv4 address.
    expect(() => setNetConfig({ allowedNetworks: ["10.0.0.0/"] })).toThrow("invalid CIDR '10.0.0.0/'");
    expect(() => setNetConfig({ blockedNetworks: ["10.0.0.0/33"] })).toThrow(TypeError);
    expect(() => setNetConfig({ blockedNetworks: ["not-an-ip/8"] })).toThrow();
  });
});

describe("Node socket transport", () => {
  let server: Server;
  let port: number;

  // Sockets the server has accepted and not yet seen close.
  const open = new Set<Socket>();

  beforeAll(async () => {
    setNetConfig(allowLoopback);
    // Upper-cases what it receives after a delay, so the host-side read
    // genuinely has to suspend. Says nothing until spoken to.
    server = createServer((sock) => {
      open.add(sock);
      sock.on("close", () => open.delete(sock));
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

  it("times out reads, capped by ioTimeoutMs", async () => {
    setNetConfig({ ...allowLoopback, ioTimeoutMs: 200 });
    try {
      const result = await sandbox.execute(`
import socket
for timeout in (0.1, None, 600):
    s = socket.create_connection(("127.0.0.1", ${port}))
    s.settimeout(timeout)
    try:
        s.recv(10)
    except OSError as e:
        print(type(e).__name__)
    s.close()
`);
      expect(result.stdout).toBe("TimeoutError\nTimeoutError\nTimeoutError\n");
    } finally {
      setNetConfig(allowLoopback);
    }
  });

  it("closes connections left open when an execution ends", async () => {
    await sandbox.execute(`
import socket
conns = [socket.create_connection(("127.0.0.1", ${port})) for _ in range(3)]
`);
    await expect.poll(() => open.size).toBe(0);
  });

  it("caps open connections at maxConnections", async () => {
    setNetConfig({ ...allowLoopback, maxConnections: 2 });
    try {
      const result = await sandbox.execute(`
import socket
conns = []
try:
    for _ in range(3):
        conns.append(socket.create_connection(("127.0.0.1", ${port})))
except OSError as e:
    print(len(conns), e)
for c in conns:
    c.close()
# Closing frees the slots again.
socket.create_connection(("127.0.0.1", ${port})).close()
print("ok")
`);
      expect(result.stdout).toMatch(/^2 .*connection limit reached \(2\)\nok\n$/);
    } finally {
      setNetConfig(allowLoopback);
    }
  });

  it("stops reading from a fast peer until the guest catches up", async () => {
    const total = 32 << 20;
    let peer: Socket | undefined;
    const flood = createServer((sock) => {
      peer = sock;
      sock.end(Buffer.alloc(total));
    });
    await new Promise<void>((r) => flood.listen(0, "127.0.0.1", r));
    try {
      const h = await nodeTcp.connect("127.0.0.1", (flood.address() as AddressInfo).port, 1000, null);
      await new Promise((r) => setTimeout(r, 300));
      // Without backpressure the host would have buffered all of it by now.
      expect(peer!.writableLength).toBeGreaterThan(0);
      let got = 0;
      for (let c; (c = await nodeTcp.read(h, 1 << 20, 5000)).length; ) got += c.length;
      expect(got).toBe(total);
      nodeTcp.close(h);
    } finally {
      flood.close();
    }
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
        if (req.url === "/redirect") {
          res.writeHead(302, { location: "http://example.com/" }).end();
          return;
        }
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
    const h = await fetchTcp.connect("127.0.0.1", port, 0, null);
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

  it("sends requests only to the connected host", async () => {
    const raw = await exchange(`GET http://127.0.0.1:${port}/abs?q=1 HTTP/1.1\r\n\r\n`);
    expect(JSON.parse(raw.split("\r\n\r\n")[1]).url).toBe("/abs?q=1");
    for (const target of ["http://example.com/", `https://127.0.0.1:${port}/`, "@example.com/"]) {
      await expect(exchange(`GET ${target} HTTP/1.1\r\n\r\n`)).rejects.toMatchObject({
        payload: { tag: "not-permitted" },
      });
    }
  });

  it("hands redirects back instead of following them", async () => {
    const raw = await exchange("GET /redirect HTTP/1.1\r\n\r\n");
    expect(raw).toMatch(/^HTTP\/1\.1 302 /);
    expect(raw).toContain("location: http://example.com/");
  });

  it("checks IP addresses against blockedNetworks", async () => {
    const isAllowed = fetchNetworkFilter({ blockedNetworks: DEFAULT_BLOCKED_NETWORKS, allowedNetworks: [] });
    await expect(fetchTcp.connect("127.0.0.1", port, 0, isAllowed)).rejects.toMatchObject({
      payload: { tag: "not-permitted", val: "host '127.0.0.1' resolves to a blocked network" },
    });
    // IPv4-mapped and NAT64 addresses are checked by the IPv4 address they carry.
    expect(isAllowed("::ffff:7f00:1", 6)).toBe(false);
    expect(isAllowed("64:ff9b::a9fe:a9fe", 6)).toBe(false);
    expect(isAllowed("::", 6)).toBe(false);
    expect(isAllowed("93.184.215.14", 4)).toBe(true);
    expect(isAllowed("2606:2800:21f:cb07:6820:80da:af6b:8b2c", 6)).toBe(true);
    const exempt = fetchNetworkFilter({ blockedNetworks: ["10.0.0.0/8"], allowedNetworks: ["10.1.0.0/16"] });
    expect(exempt("10.1.2.3", 4)).toBe(true);
    expect(exempt("10.2.0.1", 4)).toBe(false);
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
