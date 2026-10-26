/**
 * Node.js entry for the `#preview2/*` package imports.
 *
 * Under the "node" export condition, `@bytecodealliance/preview2-shim` resolves to
 * its Node.js shims, which back WASI with the host filesystem and process and lack
 * `_setFileData`. The sandbox needs the in-memory browser shims everywhere, so this
 * loads them by file URL (bypassing the package's export conditions). Browsers and
 * bundlers take the "default" branch in package.json and import them directly.
 */

async function load(name) {
  const url = import.meta.resolve(`@bytecodealliance/preview2-shim/${name}`);
  if (!url.includes("/dist/nodejs/")) {
    throw new Error(`unexpected preview2-shim layout: ${url}`);
  }
  return import(url.replace("/dist/nodejs/", "/dist/browser/"));
}

export const {
  environment,
  exit,
  stderr,
  stdin,
  stdout,
  terminalInput,
  terminalOutput,
  terminalStderr,
  terminalStdin,
  terminalStdout,
} = await load("cli");
export const { monotonicClock, wallClock } = await load("clocks");
export const { preopens, types, _setFileData, createFilesystem, InMemoryFilesystemAdapter } =
  await load("filesystem");
export const { error, poll, streams } = await load("io");
export const { random } = await load("random");
