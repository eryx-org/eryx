/**
 * Building one sandbox instance: the shared stdlib template, compiled core
 * modules and WASI imports, and per-instance filesystem and callback wiring.
 * Used by index.js in-thread and by worker.js inside a Worker.
 */

// The jco-generated Python component, transpiled with `--instantiation async`
// so each Sandbox gets its own instance and imports. The other guests are
// imported only when a sandbox uses them (see `components`).
import { instantiate as instantiatePython } from "./eryx-sandbox.js";

import {
  createCli,
  environment,
  exit,
  stdin,
  terminalInput,
  terminalOutput,
  terminalStderr,
  terminalStdin,
  terminalStdout,
} from "#preview2/cli";
import { monotonicClock, wallClock } from "#preview2/clocks";
import { createFilesystem, InMemoryFilesystemAdapter } from "#preview2/filesystem";
import { error, poll, streams } from "#preview2/io";
import { insecureSeed, random } from "#preview2/random";
import { COREUTILS_TOOLS, createProcessHost } from "./shims/process.js";
import * as sockets from "./shims/sockets.js";

// Import stdlib loader
import { loadStdlib } from "./stdlib-loader.js";

/**
 * The template file tree each new Python sandbox copies: the stdlib (added
 * once it has loaded, see {@link loadPythonStdlib}) and an empty
 * site-packages directory. Files added here are visible to sandboxes created
 * afterwards; use a Sandbox's own `_fileTree` to change one that exists.
 */
export const _fileTree = { dir: { "site-packages": { dir: {} } } };

/** @type {Promise<Object> | undefined} */
let stdlibLoad;

/**
 * Load the Python stdlib once, when the first Python sandbox starts, so an app
 * that runs only the other guests never downloads it. Every Sandbox gets its
 * own copy-on-access view of it.
 * @returns {Promise<Object>} The stdlib's directory node
 */
function loadPythonStdlib() {
  stdlibLoad ??= loadStdlib().then(
    (tree) => {
      // The stdlib tar extracts to python-stdlib/*, but the WASM expects
      // files at the root of the preopen (mounted as /python-stdlib).
      const dir = tree.dir?.["python-stdlib"] || tree;
      _fileTree.dir["python-stdlib"] = dir;
      return dir;
    },
    (e) => {
      // Let a later sandbox try again.
      stdlibLoad = undefined;
      throw e;
    },
  );
  return stdlibLoad;
}

/**
 * Copy a file tree for one instance. Directories are copied up front; a file's
 * bytes are copied the first time the instance touches them, so writes (which
 * the in-memory filesystem makes in place) never reach the template or another
 * instance, and untouched stdlib files cost nothing.
 * @param {Object} node
 * @returns {Object}
 */
export function forkTree(node) {
  if (node.dir) {
    const dir = {};
    for (const [name, child] of Object.entries(node.dir)) dir[name] = forkTree(child);
    return { ...node, dir };
  }
  if (!("source" in node)) return { ...node };
  const shared = node.source;
  let own;
  return {
    get source() {
      // Not `slice()`: on a Node Buffer that returns a view, not a copy.
      return (own ??= typeof shared === "string" ? shared : new Uint8Array(shared));
    },
    set source(value) {
      own = value;
    },
  };
}

/**
 * A new sandbox's own file tree: for Python a copy of {@link _fileTree}, for
 * the other guests an empty root.
 * @param {string} language
 * @returns {Object}
 */
export function newFileTree(language) {
  return language === "python" ? forkTree(_fileTree) : { dir: {} };
}

export const isNode =
  typeof globalThis.process !== "undefined" &&
  typeof globalThis.process.versions?.node !== "undefined";

/** Compiled core modules by URL, shared by every instance. */
const _coreModules = new Map();

/**
 * Compile (once) the core wasm module at `url`.
 * @param {URL} url
 * @returns {Promise<WebAssembly.Module>}
 */
function compileAt(url) {
  const key = url.href;
  if (!_coreModules.has(key)) {
    const module = isNode
      ? import(/* webpackIgnore: true */ "node:fs/promises").then(async (fs) =>
          WebAssembly.compile(await fs.readFile(url)),
        )
      : WebAssembly.compileStreaming(fetch(url));
    // Don't keep a failure (e.g. a dropped download): the next sandbox retries.
    module.catch(() => _coreModules.delete(key));
    _coreModules.set(key, module);
  }
  return _coreModules.get(key);
}

/**
 * Compile (once) a core wasm module of this package's components.
 * @param {string} name
 * @returns {Promise<WebAssembly.Module>}
 */
export function getCoreModule(name) {
  return compileAt(new URL(`./${name}`, import.meta.url));
}

/** A command module (jco output) by URL, loaded once. */
const _commandModules = new Map();

/**
 * The loaders for a shell sandbox's external commands: the bundled coreutils
 * (downloaded the first time one runs) unless `coreutils` is false, then
 * `commands`, which replace coreutils tools of the same name.
 * @param {{ coreutils?: boolean, commands?: Record<string, string> }} config -
 *   `commands` maps names to the URL of a jco-transpiled command's `.js`
 * @param {(name: string) => WebAssembly.Module | Promise<WebAssembly.Module>} getModule -
 *   Source of this package's core modules
 * @returns {Map<string, () => Promise<import("./shims/process.js").CommandModule>>}
 */
export function commandLoaders(config, getModule) {
  const loaders = new Map();
  if (config.coreutils !== false) {
    const load = async () => ({
      instantiate: (await import("./eryx-coreutils.js")).instantiate,
      getCoreModule: getModule,
    });
    for (const tool of COREUTILS_TOOLS) loaders.set(tool, load);
  }
  for (const [name, href] of Object.entries(config.commands ?? {})) {
    loaders.set(name, () => {
      if (!_commandModules.has(href)) {
        const loaded = import(/* webpackIgnore: true */ href).then((module) => ({
          instantiate: module.instantiate,
          getCoreModule: (file) => compileAt(new URL(file, href)),
        }));
        loaded.catch(() => _commandModules.delete(href));
        _commandModules.set(href, loaded);
      }
      return _commandModules.get(href);
    });
  }
  return loaders;
}

const _sockets = {
  "wasi:sockets/instance-network": sockets.instanceNetwork,
  "wasi:sockets/ip-name-lookup": sockets.ipNameLookup,
  "wasi:sockets/network": sockets.network,
  "wasi:sockets/tcp": sockets.tcp,
  "wasi:sockets/tcp-create-socket": sockets.tcpCreateSocket,
  "wasi:sockets/udp": sockets.udp,
  "wasi:sockets/udp-create-socket": sockets.udpCreateSocket,
};

/**
 * Most bytes kept per stream per execution from writes to fds 1 and 2, as the
 * Rust host's `FD_CAPTURE_LIMIT`; the rest is dropped.
 */
const FD_CAPTURE_LIMIT = 1024 * 1024;

/**
 * Collect an instance's writes to WASI fds 1 and 2 (the shell guest's output,
 * Python's `os.write(1, ...)` and `sys.__stdout__`), as the Rust host does,
 * instead of printing them to the console.
 */
function createFdCapture() {
  const chunks = [[], []];
  const sizes = [0, 0];
  const handler = (stream) => ({
    write(buf) {
      const room = FD_CAPTURE_LIMIT - sizes[stream];
      if (room <= 0) return;
      // Copied: the buffer is a view of the guest's memory.
      const kept = buf.slice(0, room);
      chunks[stream].push(kept);
      sizes[stream] += kept.byteLength;
    },
  });
  const cli = createCli({ stdout: handler(0), stderr: handler(1) });
  /** Remove and return what stream `stream` captured since the last take. */
  const take = (stream) => {
    const out = new Uint8Array(sizes[stream]);
    let offset = 0;
    for (const chunk of chunks[stream]) {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    }
    chunks[stream] = [];
    sizes[stream] = 0;
    return out;
  };
  return { stdout: cli.stdout, stderr: cli.stderr, take };
}

/** Decodes fd 2 bytes for a failure's message, replacing invalid UTF-8 (as `from_utf8_lossy`). */
const fdDecoder = new TextDecoder();

/** `a` followed by `b`. */
function concat(a, b) {
  if (b.byteLength === 0) return a;
  const out = new Uint8Array(a.byteLength + b.byteLength);
  out.set(a);
  out.set(b, a.byteLength);
  return out;
}

/**
 * The `instantiate` of each guest's jco-generated component, by language.
 * JavaScript and shell are separate modules, loaded on first use so a
 * Python-only app never downloads them. A `Map`, so that a language name
 * can't reach `Object.prototype`.
 * @type {Map<string, () => Promise<Function>>}
 */
const components = new Map([
  ["python", async () => instantiatePython],
  ["javascript", async () => (await import("./eryx-sandbox-javascript.js")).instantiate],
  ["shell", async () => (await import("./eryx-sandbox-shell.js")).instantiate],
]);

/** The guest languages this package runs. */
export const languages = [...components.keys()];

/**
 * Instantiate a guest's component with its own filesystem and callback
 * imports, then (for Python) finish the pre-initialized interpreter's setup.
 * @param {string} language - One of {@link languages}
 * @param {Object} fileTree - The instance's own file tree
 * @param {ReturnType<typeof import("./shims/callbacks.js").createCallbackHost>["imports"]} cb - Callback imports
 * @param {{tcp: Object, tls: Object}} net - The instance's eryx:net imports
 * @param {(name: string) => WebAssembly.Module | Promise<WebAssembly.Module>} [getModule] -
 *   Core-module source, when not this thread's cache
 * @param {{ coreutils?: boolean, commands?: Record<string, string> }} [commands] - The
 *   shell's external commands; see {@link commandLoaders}
 */
export async function instantiateSandbox(
  language,
  fileTree,
  cb,
  net,
  getModule = getCoreModule,
  commands = {},
) {
  // A tree forked before the stdlib loaded gets its own view of it, attached
  // before the guest's first filesystem access (finalizePreinit, below):
  // instantiation doesn't read the filesystem, so the two overlap.
  const stdlib =
    language === "python" && !fileTree.dir["python-stdlib"] ? loadPythonStdlib() : undefined;
  stdlib?.catch(() => {}); // surfaced by the await below
  const load = components.get(language);
  if (!load) throw new TypeError(`unknown language ${JSON.stringify(language)}`);
  const instantiate = await load();
  const fd = createFdCapture();
  const processHost = createProcessHost(fileTree, commandLoaders(commands, getModule));
  // When the running execution times out (`Date.now()` ms), if it can.
  let deadline;
  const execution = {
    timeLeft: () =>
      deadline === undefined
        ? undefined
        : BigInt(Math.max(0, Math.round((deadline - Date.now()) * 1e6))),
    // A child here runs as one call that can't be stopped midway (see
    // shims/process.js), so there's nothing to kill.
    killChildren: () => {},
  };
  const fs = createFilesystem({
    adapter: new InMemoryFilesystemAdapter(),
    preopens: { "/": fileTree },
  });
  const instance = await instantiate(getModule, {
    invoke: { default: cb.invoke },
    sleep: { default: cb.sleep },
    "list-callbacks": { default: cb.listCallbacks },
    "get-execution-options": { default: cb.getExecutionOptions },
    "report-trace": { default: cb.reportTrace },
    "report-output": { default: cb.reportOutput },
    "eryx:net/tcp": net.tcp,
    "eryx:net/tls": net.tls,
    // Only the shell imports it; see shims/process.js.
    "eryx:process/process": processHost,
    // The shell's background jobs stop in time for the deadline.
    "eryx:process/execution": execution,
    "wasi:cli/environment": environment,
    "wasi:cli/exit": exit,
    "wasi:cli/stderr": fd.stderr,
    "wasi:cli/stdin": stdin,
    "wasi:cli/stdout": fd.stdout,
    "wasi:cli/terminal-input": terminalInput,
    "wasi:cli/terminal-output": terminalOutput,
    "wasi:cli/terminal-stderr": terminalStderr,
    "wasi:cli/terminal-stdin": terminalStdin,
    "wasi:cli/terminal-stdout": terminalStdout,
    "wasi:clocks/monotonic-clock": monotonicClock,
    "wasi:clocks/wall-clock": wallClock,
    "wasi:filesystem/preopens": fs.preopens,
    "wasi:filesystem/types": fs.types,
    "wasi:io/error": error,
    "wasi:io/poll": poll,
    "wasi:io/streams": streams,
    "wasi:random/insecure-seed": insecureSeed,
    "wasi:random/random": random,
    ..._sockets,
  });
  // The pre-initialized WASM has Python's core state baked in, but
  // finalizePreinit() must still be called to finish setup.
  if (stdlib) fileTree.dir["python-stdlib"] ??= forkTree(await stdlib);
  if (language === "python") instance.finalizePreinit();
  fd.take(0);
  fd.take(1);
  /**
   * Append the run's fd 1/2 output to both arms, and stream it once. As the
   * Rust host's `append_fd_stderr`, a failure's message also gets fd 2.
   */
  const appendFdOutput = (output, failed = false) => {
    for (const [stream, key] of [
      [0, "stdout"],
      [1, "stderr"],
    ]) {
      const data = fd.take(stream);
      if (data.byteLength === 0) continue;
      output[key] = concat(output[key], data);
      if (failed && stream === 1) output.message += fdDecoder.decode(data);
      cb.reportOutput(stream, data);
    }
  };
  return {
    ...instance,
    /**
     * @param {string} code
     * @param {number} [deadlineMs] - When the execution times out, as `Date.now()`.
     */
    async execute(code, deadlineMs) {
      processHost.startExecution();
      deadline = deadlineMs;
      let output;
      try {
        output = await instance.execute(code);
      } catch (e) {
        if (e?.payload) appendFdOutput(e.payload, true);
        throw e;
      } finally {
        deadline = undefined;
      }
      appendFdOutput(output);
      return output;
    },
  };
}


/**
 * Turn a thrown value into something structured clone keeps intact, keeping
 * the `payload` (a failed run's execute-error record) jco attaches.
 */
export function encodeError(e) {
  return e instanceof Error
    ? { error: { name: e.name, message: e.message, payload: e.payload } }
    : { thrown: e };
}

/** Rebuild a value thrown on the other side of the channel. */
export function decodeError(reply) {
  if (!reply.error) return reply.thrown;
  const error = new Error(reply.error.message);
  error.name = reply.error.name;
  if (reply.error.payload !== undefined) error.payload = reply.error.payload;
  return error;
}
