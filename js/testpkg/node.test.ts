import { describe, expect, it } from "vitest";
import { execFileSync } from "node:child_process";

describe("plain Node.js", () => {
  // Runs outside vitest's module resolution, as a consumer's `node app.js` would.
  it("imports the package and runs code on the in-memory filesystem", () => {
    const script = `
      const { execute } = await import("@bsull/eryx");
      const { stdout } = await execute("import os; print(sorted(os.listdir('/')))");
      process.stdout.write(stdout);
    `;
    const stdout = execFileSync(
      process.execPath,
      ["--experimental-wasm-jspi", "--input-type=module", "-e", script],
      { cwd: import.meta.dirname, encoding: "utf8" },
    );
    expect(stdout).toBe("['python-stdlib', 'site-packages']\n");
  });
});
