import { describe, it, expect } from "vitest";
import { build, createServer, preview } from "vite";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { existsSync, readdirSync, readFileSync } from "fs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");

describe("demo build", () => {
  it("produces dist/ with expected assets", async () => {
    await build({ root, logLevel: "silent" });

    const distDir = resolve(root, "dist");
    expect(existsSync(distDir)).toBe(true);
    expect(existsSync(resolve(distDir, "index.html"))).toBe(true);

    const assets = readdirSync(resolve(distDir, "assets"));
    expect(assets.some((f: string) => f.endsWith(".css"))).toBe(true);
    expect(assets.some((f: string) => f.endsWith(".js"))).toBe(true);
    expect(assets.some((f: string) => f.endsWith(".wasm"))).toBe(true);
    expect(assets.some((f: string) => f.includes(".gz"))).toBe(true);

    // Verify WASM files are brotli-compressed (won't start with WASM magic \0asm)
    const wasmFiles = assets.filter((f: string) => f.endsWith(".wasm"));
    for (const wf of wasmFiles) {
      const bytes = readFileSync(resolve(distDir, "assets", wf));
      expect(bytes[0]).not.toBe(0x00); // not raw WASM
    }

    // Verify _headers file sets Content-Encoding and no-transform
    const headers = readFileSync(resolve(distDir, "_headers"), "utf-8");
    expect(headers).toContain("Content-Encoding: br");
    expect(headers).toContain("no-transform");
  });

  it("serves .tar.gz without Content-Encoding in preview mode", async () => {
    // Build first (may already be built from previous test)
    await build({ root, logLevel: "silent" });

    const server = await preview({ root, preview: { port: 0 } });
    const address = server.httpServer!.address();
    const port =
      typeof address === "object" && address ? address.port : 4173;
    const base = `http://localhost:${port}`;

    try {
      // Find the hashed .gz file in dist/assets/
      const assets = readdirSync(resolve(root, "dist/assets"));
      const gzFile = assets.find((f: string) => f.endsWith(".gz"));
      expect(gzFile).toBeDefined();

      const gzResp = await fetch(`${base}/assets/${gzFile}`);
      expect(gzResp.status).toBe(200);
      expect(gzResp.headers.get("content-encoding")).toBeNull();
      expect(gzResp.headers.get("content-type")).toBe("application/gzip");
      const gzBytes = new Uint8Array(await gzResp.arrayBuffer());
      expect(gzBytes[0]).toBe(0x1f);
      expect(gzBytes[1]).toBe(0x8b);

      // Verify brotli-compressed WASM files are served with Content-Encoding: br
      const wasmFile = assets.find((f: string) => f.endsWith(".wasm"));
      expect(wasmFile).toBeDefined();
      const wasmResp = await fetch(`${base}/assets/${wasmFile}`);
      expect(wasmResp.status).toBe(200);
      expect(wasmResp.headers.get("content-encoding")).toBe("br");
      expect(wasmResp.headers.get("content-type")).toBe("application/wasm");
    } finally {
      server.httpServer!.close();
    }
  });
});

describe("demo dev server", () => {
  it("starts and serves the app without import errors", async () => {
    const server = await createServer({ root, logLevel: "silent" });
    await server.listen();

    const address = server.httpServer!.address();
    const port =
      typeof address === "object" && address ? address.port : 5173;
    const base = `http://localhost:${port}`;

    try {
      // Fetch the index page
      const html = await fetch(base).then((r) => r.text());
      expect(html).toContain('<div id="app">');
      expect(html).toContain("src/main.ts");

      // Fetch main.ts through Vite's transform pipeline
      const mainJs = await fetch(`${base}/src/main.ts`).then((r) => {
        expect(r.status).toBe(200);
        return r.text();
      });
      expect(mainJs).toContain("App.svelte");

      // Verify the eryx package's index.js can be transformed
      // (tests both fs.allow and the preview2-shim alias)
      const transformedEryx = await server.transformRequest(
        resolve(root, "../eryx-sandbox/index.js"),
      );
      expect(transformedEryx).not.toBeNull();

      // Verify .tar.gz files are served as raw gzip (no Content-Encoding)
      // so that DecompressionStream("gzip") in stdlib-loader.js works
      const stdlibPath = resolve(root, "../eryx-sandbox/python-stdlib.tar.gz");
      const gzResp = await fetch(`${base}/@fs${stdlibPath}`);
      expect(gzResp.status).toBe(200);
      expect(gzResp.headers.get("content-encoding")).toBeNull();
      expect(gzResp.headers.get("content-type")).toBe("application/gzip");
      // Verify the response is still gzip-compressed (magic bytes 0x1f 0x8b)
      const gzBytes = new Uint8Array(await gzResp.arrayBuffer());
      expect(gzBytes[0]).toBe(0x1f);
      expect(gzBytes[1]).toBe(0x8b);
    } finally {
      // Force-close: the optimizer may still be running in the background,
      // causing server.close() to hang. Kill the HTTP server directly.
      server.httpServer!.close();
    }
  });
});
