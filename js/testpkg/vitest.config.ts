import { defineConfig } from "vitest/config";
import { resolve } from "path";

// Resolve paths to the browser shims in eryx's node_modules
const shimBase = resolve(
  __dirname,
  "../eryx/node_modules/@bytecodealliance/preview2-shim/lib/browser",
);

export default defineConfig({
  resolve: {
    alias: {
      // Force browser shims for @bytecodealliance/preview2-shim
      // This ensures the VFS-compatible browser shims are used even in Node.js
      "@bytecodealliance/preview2-shim/cli": resolve(shimBase, "cli.js"),
      "@bytecodealliance/preview2-shim/clocks": resolve(shimBase, "clocks.js"),
      "@bytecodealliance/preview2-shim/filesystem": resolve(
        shimBase,
        "filesystem.js",
      ),
      "@bytecodealliance/preview2-shim/io": resolve(shimBase, "io.js"),
      "@bytecodealliance/preview2-shim/random": resolve(shimBase, "random.js"),
    },
  },
  test: {
    environment: "node",
    // Run tests serially since sandbox state is global
    pool: "forks",
    poolOptions: {
      forks: {
        singleFork: true,
        // Enable JSPI for async WASM component model support
        execArgv: ["--experimental-wasm-jspi"],
      },
    },
    sequence: {
      shuffle: false,
    },
    exclude: ["**/node_modules/**"],
    // Python sandbox initialization can be slow
    testTimeout: 30000,
  },
});
