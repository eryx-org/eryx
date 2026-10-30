import { defineConfig } from "vitest/config";

export default defineConfig({
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
