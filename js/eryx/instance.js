/**
 * Building one sandbox instance: the shared stdlib template, compiled core
 * modules and WASI imports, and per-instance filesystem and callback wiring.
 * Used by index.js in-thread and by worker.js inside a Worker.
 */

// The jco-generated component, transpiled with `--instantiation async` so each
// Sandbox gets its own instance and imports.
import { instantiate } from "./eryx-sandbox.js";

import {
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
} from "#preview2/cli";
import { monotonicClock, wallClock } from "#preview2/clocks";
import { createFilesystem, InMemoryFilesystemAdapter } from "#preview2/filesystem";
import { error, poll, streams } from "#preview2/io";
import { random } from "#preview2/random";
import * as sockets from "./shims/sockets.js";

// Import stdlib loader
import { loadStdlib } from "./stdlib-loader.js";

// Load the Python stdlib once; every Sandbox gets its own copy-on-access view of it.
const stdlibTree = await loadStdlib();

// The stdlib tar extracts to python-stdlib/*, but the WASM expects
// files at the root of the preopen (mounted as /python-stdlib).
const stdlibDir = stdlibTree.dir?.["python-stdlib"] || stdlibTree;

/**
 * The template file tree each new Sandbox copies: the stdlib and an empty
 * site-packages directory. Files added here are visible to sandboxes created
 * afterwards; use a Sandbox's own `_fileTree` to change one that exists.
 */
export const _fileTree = {
  dir: { "python-stdlib": stdlibDir, "site-packages": { dir: {} } },
};

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

export const isNode =
  typeof globalThis.process !== "undefined" &&
  typeof globalThis.process.versions?.node !== "undefined";

/** Compiled core modules, shared by every instance. */
const _coreModules = new Map();

/**
 * Compile (once) a core wasm module of the component.
 * @param {string} name
 * @returns {Promise<WebAssembly.Module>}
 */
export function getCoreModule(name) {
  if (!_coreModules.has(name)) {
    const url = new URL(`./${name}`, import.meta.url);
    _coreModules.set(
      name,
      isNode
        ? import(/* webpackIgnore: true */ "node:fs/promises").then(async (fs) =>
            WebAssembly.compile(await fs.readFile(url)),
          )
        : WebAssembly.compileStreaming(fetch(url)),
    );
  }
  return _coreModules.get(name);
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
 * Instantiate the component with its own filesystem and callback imports, then
 * finish the pre-initialized interpreter's setup.
 * @param {Object} fileTree - The instance's own file tree
 * @param {ReturnType<typeof import("./shims/callbacks.js").createCallbackHost>["imports"]} cb - Callback imports
 * @param {{tcp: Object, tls: Object}} net - The instance's eryx:net imports
 * @param {(name: string) => WebAssembly.Module | Promise<WebAssembly.Module>} [getModule] -
 *   Core-module source, when not this thread's cache
 */
export async function instantiateSandbox(fileTree, cb, net, getModule = getCoreModule) {
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
    "wasi:cli/environment": environment,
    "wasi:cli/exit": exit,
    "wasi:cli/stderr": stderr,
    "wasi:cli/stdin": stdin,
    "wasi:cli/stdout": stdout,
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
    "wasi:random/random": random,
    ..._sockets,
  });
  // The pre-initialized WASM has Python's core state baked in, but
  // finalizePreinit() must still be called to finish setup.
  instance.finalizePreinit();
  return instance;
}


/** Turn a thrown value into something structured clone keeps intact. */
export function encodeError(e) {
  return e instanceof Error ? { error: { name: e.name, message: e.message } } : { thrown: e };
}

/** Rebuild a value thrown on the other side of the channel. */
export function decodeError(reply) {
  if (!reply.error) return reply.thrown;
  const error = new Error(reply.error.message);
  error.name = reply.error.name;
  return error;
}
