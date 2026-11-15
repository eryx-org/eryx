import { describe, it, expect } from "vitest";
import { createServer } from "vite";
import { chromium } from "playwright";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

describe("in Chromium", () => {
  // A browser drops a message that reaches a module Worker before its
  // listener exists, which Node.js queues, so only a browser catches this.
  it("runs timeoutMs sandboxes in every language", async () => {
    const server = await createServer({ root, logLevel: "silent", server: { port: 0 } });
    await server.listen();
    const address = server.httpServer!.address();
    const port = typeof address === "object" && address ? address.port : 5173;
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      await page.goto(`http://localhost:${port}/test/worker-timeout.html`);
      await page.waitForFunction(() => "results" in window, null, { timeout: 120000 });
      const results = await page.evaluate(() => (window as any).results);
      expect(results).toEqual({ python: "ok\n", javascript: "ok\n", shell: "ok\n" });
    } finally {
      await browser.close();
      await server.close();
    }
  }, 180000);
});
