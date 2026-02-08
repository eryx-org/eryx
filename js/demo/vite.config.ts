import { defineConfig } from "vite";
import type { Connect } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { fileURLToPath } from "url";
import { dirname, resolve } from "path";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "fs";
import { brotliCompressSync, constants } from "zlib";

const __dirname = dirname(fileURLToPath(import.meta.url));
const shimDir = resolve(
  __dirname,
  "node_modules/@bytecodealliance/preview2-shim/lib/browser",
);

/** Intercept .gz requests and serve without Content-Encoding so the browser
 *  doesn't auto-decompress (stdlib-loader uses DecompressionStream itself). */
const serveGzipRaw: Connect.NextHandleFunction = (req, res, next) => {
  const url = req.url ?? "";
  if (!url.endsWith(".tar.gz") && !url.endsWith(".gz")) return next();

  // Dev server uses /@fs/ for files outside project root;
  // preview server serves from dist/
  let filePath: string;
  if (url.startsWith("/@fs/")) {
    filePath = url.slice("/@fs".length);
  } else {
    filePath = resolve(__dirname, url.slice(1));
    if (!existsSync(filePath)) {
      filePath = resolve(__dirname, "dist", url.slice(1));
    }
  }

  if (!existsSync(filePath)) return next();

  const content = readFileSync(filePath);
  res.setHeader("Content-Type", "application/gzip");
  res.setHeader("Content-Length", content.length.toString());
  res.removeHeader("Content-Encoding");
  res.end(content);
};

export default defineConfig({
  plugins: [
    svelte(),
    // Serve .tar.gz files as raw binary without Content-Encoding: gzip.
    // Vite's static middleware sets Content-Encoding: gzip for .gz files,
    // causing the browser to auto-decompress them. The stdlib loader expects
    // to decompress the gzip itself via DecompressionStream.
    {
      name: "serve-gzip-raw",
      configureServer(server) {
        server.middlewares.use(serveGzipRaw);
      },
      configurePreviewServer(server) {
        server.middlewares.use(serveGzipRaw);
      },
    },
    // Brotli-compress .wasm files and write a Cloudflare Pages _headers file.
    // We compress ourselves because some WASM cores exceed CF Pages' 25 MiB
    // file limit. The _headers file sets Content-Encoding: br (so browsers
    // decompress before WebAssembly.compileStreaming()) and Cache-Control:
    // no-transform (so CF's CDN proxy on custom domains doesn't decompress
    // and re-compress on the fly, which causes multi-minute load times).
    {
      name: "brotli-wasm",
      closeBundle() {
        const assetsDir = resolve(__dirname, "dist/assets");
        if (!existsSync(assetsDir)) return;
        for (const file of readdirSync(assetsDir)) {
          if (!file.endsWith(".wasm")) continue;
          const filePath = resolve(assetsDir, file);
          const raw = readFileSync(filePath);
          writeFileSync(
            filePath,
            brotliCompressSync(raw, {
              params: { [constants.BROTLI_PARAM_QUALITY]: 9 },
            }),
          );
        }
        writeFileSync(
          resolve(__dirname, "dist/_headers"),
          `/assets/*.wasm\n  Content-Encoding: br\n  Cache-Control: public, no-transform\n`,
        );
      },
      // In preview mode, serve brotli-compressed .wasm with Content-Encoding: br
      // so the browser decompresses before passing to WebAssembly.compileStreaming().
      // (CF Pages handles this via _headers, but vite preview doesn't read that.)
      configurePreviewServer(server) {
        server.middlewares.use((req, res, next) => {
          const url = req.url ?? "";
          if (!url.endsWith(".wasm")) return next();
          const filePath = resolve(__dirname, "dist", url.slice(1));
          if (!existsSync(filePath)) return next();
          const content = readFileSync(filePath);
          res.setHeader("Content-Type", "application/wasm");
          res.setHeader("Content-Encoding", "br");
          res.setHeader("Content-Length", content.length.toString());
          res.end(content);
        });
      },
    },
  ],
  resolve: {
    alias: [
      // Vite resolves bare imports relative to the importing file. Since
      // @bsull/eryx is symlinked from ../eryx/ (outside demo/),
      // Vite can't find preview2-shim from there. Alias it to our copy.
      {
        find: /^@bytecodealliance\/preview2-shim\/(.+)$/,
        replacement: resolve(shimDir, "$1.js"),
      },
    ],
  },
  build: {
    // We require JSPI (Chrome 133+), so modern targets are fine.
    // esnext is needed for top-level await in the eryx package.
    target: "esnext",
    // Disable compressed size reporting - slow for large WASM files and
    // misleading since we pre-compress with Brotli.
    reportCompressedSize: false,
  },
  optimizeDeps: {
    // Don't pre-bundle eryx during dev - it has top-level await and WASM loads
    exclude: ["@bsull/eryx"],
  },
  server: {
    headers: {
      // Required for SharedArrayBuffer (used by WASM JSPI)
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
    },
    fs: {
      // Allow serving WASM/JS files from the eryx package (symlinked
      // from outside demo/ via the file: dependency). Setting allow
      // disables automatic workspace root detection, so include "." too.
      allow: [".", resolve(__dirname, "../eryx")],
    },
  },
});
