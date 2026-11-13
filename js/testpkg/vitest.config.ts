import { defineConfig } from "vitest/config";

export default defineConfig({
  // @eryx-org/eryx is linked from ../eryx with its own node_modules; resolve
  // its dependency on the client to this package's copy, as an npm install
  // (which dedupes) would, so its classes are the ones the tests import
  resolve: { dedupe: ["@eryx-org/eryx-run"] },
  test: {
    environment: "node",
    // Run tests serially since sandbox state is global
    maxWorkers: 1,
    sequence: {
      shuffle: false,
    },
    exclude: ["**/node_modules/**"],
    // Python sandbox initialization can be slow
    testTimeout: 30000,
  },
});
