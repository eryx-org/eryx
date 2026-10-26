/**
 * Eryx - JavaScript API
 *
 * This module provides a high-level API for the eryx Python sandbox
 * WebAssembly component.
 *
 * Usage:
 *   ```js
 *   import { Sandbox } from '@bsull/eryx';
 *   const sandbox = new Sandbox();
 *   const result = await sandbox.execute('print("hello")');
 *   console.log(result.stdout);  // "hello\n"
 *   ```
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
import { tcp, tls } from "./shims/net.js";
import { createCallbackHost } from "./shims/callbacks.js";

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
function forkTree(node) {
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

const isNode =
  typeof globalThis.process !== "undefined" &&
  typeof globalThis.process.versions?.node !== "undefined";

/** Compiled core modules, shared by every instance. */
const _coreModules = new Map();

/**
 * Compile (once) a core wasm module of the component.
 * @param {string} name
 * @returns {Promise<WebAssembly.Module>}
 */
function getCoreModule(name) {
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
 * @param {ReturnType<typeof createCallbackHost>} host
 */
async function _instantiate(fileTree, host) {
  const fs = createFilesystem({
    adapter: new InMemoryFilesystemAdapter(),
    preopens: { "/": fileTree },
  });
  const cb = host.imports;
  const instance = await instantiate(getCoreModule, {
    invoke: { default: cb.invoke },
    sleep: { default: cb.sleep },
    "list-callbacks": { default: cb.listCallbacks },
    "get-execution-options": { default: cb.getExecutionOptions },
    "report-trace": { default: cb.reportTrace },
    "report-output": { default: cb.reportOutput },
    "eryx:net/tcp": tcp,
    "eryx:net/tls": tls,
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

// Re-export callback configuration
export {
  setCallbackHandler,
  setCallbacks,
  setTraceHandler,
  setOutputHandler,
  SuspendCallback,
} from "./shims/callbacks.js";

// Re-export network policy configuration
export { setNetConfig, DEFAULT_BLOCKED_HOSTS, DEFAULT_BLOCKED_NETWORKS } from "./shims/net.js";

/**
 * Result from executing Python code in the sandbox.
 * @typedef {Object} ExecuteResult
 * @property {string} stdout - Captured standard output
 * @property {string} stderr - Captured standard error
 * @property {*} [result] - The script's `result` variable, parsed from JSON, or
 *   undefined if it was not set. Integers outside the safe range are parsed as
 *   BigInt to avoid precision loss (so a numeric field may be a number or a
 *   bigint depending on magnitude). See {@link setResultVariable} to change the name.
 * @property {string} [resultJson] - The raw JSON string of the captured result, or
 *   undefined if not set. Use this when you need exact values (e.g. large integers)
 *   and want to parse them yourself.
 * @property {string} [resultError] - Why result capture failed (e.g. the value was
 *   not JSON-serializable), or undefined on success.
 */

/**
 * Parse the captured result JSON, promoting integers outside the IEEE-754 safe
 * range to BigInt so they round-trip without precision loss.
 *
 * Uses the ES2023 reviver `context.source` (the raw source text of the value) to
 * distinguish a true integer literal from a float. When the runtime doesn't
 * provide it (older engines), falls back to a plain (lossy) parse.
 * @param {string} json
 * @returns {*}
 */
function _parseResultJson(json) {
  return JSON.parse(json, (_key, value, context) => {
    if (
      typeof value === "number" &&
      Number.isInteger(value) &&
      !Number.isSafeInteger(value) &&
      context &&
      typeof context.source === "string" &&
      /^-?\d+$/.test(context.source)
    ) {
      return BigInt(context.source);
    }
    return value;
  });
}

/**
 * Map a raw jco execute-output record into the public ExecuteResult shape,
 * parsing the captured result variable from JSON.
 * @param {Object} output
 * @returns {ExecuteResult}
 */
const _decoder = new TextDecoder("utf-8", { fatal: false, ignoreBOM: true });

function _toResult(output) {
  return {
    stdout: _decoder.decode(output.stdout),
    stderr: _decoder.decode(output.stderr),
    result: output.resultJson ? _parseResultJson(output.resultJson) : undefined,
    resultJson: output.resultJson || undefined,
    resultError: output.resultError || undefined,
  };
}

/**
 * The outcome of executeWithJournal().
 * @typedef {Object} ReplayOutcome
 * @property {ExecuteResult} [result] - The execution result, or undefined if it failed
 * @property {Error} [error] - Why execution failed (Python exception, or the
 *   SuspendCallback that halted it), or undefined on success
 * @property {string} journal - Callbacks completed during this run, serialized
 *   as JSON in the same shape as the Rust `CallbackJournal`. Always present, even
 *   on error. Store it as-is and pass it back via `options.journal`.
 * @property {number} replayedCallbacks - How many callbacks were served from
 *   the supplied journal instead of running live
 * @property {{name: string, argsJson: string, reason: string}} [suspended] -
 *   Set if a callback threw SuspendCallback
 */

/**
 * Options for a {@link Sandbox}. Each handler option overrides, for this
 * sandbox only, the module-wide default set by the setter of the same name
 * (`setCallbackHandler()` and so on); an omitted option follows that default.
 * @typedef {Object} SandboxOptions
 * @property {((name: string, argsJson: string) => string | Promise<string>) | null} [callbackHandler]
 * @property {Array<{name: string, description: string, parametersSchemaJson?: string}>} [callbacks]
 * @property {((lineno: number, eventJson: string, contextJson: string) => void) | null} [traceHandler]
 * @property {((stream: number, data: string) => void) | null} [outputHandler]
 */

/**
 * A Python sandbox powered by WebAssembly.
 *
 * Each Sandbox is its own WebAssembly instance, with its own Python
 * interpreter, filesystem, callback handlers and journal, so code run in one
 * can't see or change another's state. Python state (variables, imports,
 * etc.) persists across execute() calls on the same Sandbox.
 *
 * The compiled code is shared, but every instance has its own linear memory
 * (the pre-initialized interpreter image plus whatever the code allocates), so
 * each live Sandbox costs tens of megabytes. Drop references to sandboxes you
 * no longer need so they can be garbage collected.
 *
 * @example
 * const sandbox = new Sandbox();
 *
 * // Variables persist across calls
 * await sandbox.execute('x = 42');
 * const result = await sandbox.execute('print(x)');
 * console.log(result.stdout);  // "42"
 *
 * // Reset state
 * await sandbox.clearState();
 */
export class Sandbox {
  /** @type {ReturnType<typeof createCallbackHost>} */
  #host;
  /** @type {Promise<Awaited<ReturnType<typeof _instantiate>>>} */
  #instance;
  // The tail of the queue of calls into this instance. Each call starts only
  // after the previous one settles, so executions never overlap: the journal
  // and the streaming output decoders belong to the instance, and jco can't
  // run two calls on one instance at once.
  #queue = Promise.resolve();

  /**
   * Create a sandbox. Instantiation starts right away and finishes in the
   * background; the first call waits for it, and rejects if it failed.
   *
   * @param {SandboxOptions} [options]
   */
  constructor(options = {}) {
    /**
     * This sandbox's own file tree (a copy of the module's `_fileTree`).
     * Changes to it are visible to the sandbox's next filesystem access.
     * @internal For the demo app and advanced integrations.
     */
    this._fileTree = forkTree(_fileTree);
    this.#host = createCallbackHost(options);
    this.#instance = _instantiate(this._fileTree, this.#host);
    // Surfaced by the first call instead.
    this.#instance.catch(() => {});
  }

  /**
   * Run `fn` on the instance once every call queued before it has settled.
   * @template T
   * @param {(instance: Awaited<ReturnType<typeof _instantiate>>) => Promise<T>} fn
   * @returns {Promise<T>}
   */
  #serialized(fn) {
    const run = this.#queue.then(() => this.#instance).then(fn);
    this.#queue = run.catch(() => {});
    return run;
  }

  /**
   * Run code, flushing streamed output when it finishes. Callers must hold the
   * queue, so the flush can't cut into another execution's output.
   * @param {Awaited<ReturnType<typeof _instantiate>>} instance
   * @param {string} code
   * @returns {Promise<ExecuteResult>}
   */
  async #run(instance, code) {
    try {
      // Frame the code with an empty preamble (see the guest's PREAMBLE_HEADER),
      // so a leading header-like line in user code isn't misparsed.
      return _toResult(await instance.execute(`#eryx-preamble:0\n${code}`));
    } finally {
      this.#host.flushOutput();
    }
  }

  /**
   * Execute Python code in the sandbox.
   *
   * The code runs in the sandboxed Python interpreter. Output to stdout
   * and stderr is captured and returned. Variables and imports persist
   * across calls on the same Sandbox instance.
   *
   * Calls on one Sandbox run one at a time, in call order; an execute() made
   * while another call on it is running waits for it to finish. Don't await
   * execute() on the same Sandbox from inside its callback handler: it would
   * wait for the execution that invoked the handler, which never finishes.
   *
   * @param {string} code - Python source code to execute
   * @returns {Promise<ExecuteResult>} Captured stdout and stderr
   * @throws {Error} If the Python code raises an unhandled exception
   *
   * @example
   * const result = await sandbox.execute('print("hello")');
   * console.log(result.stdout);  // "hello\n"
   */
  async execute(code) {
    return this.#serialized((instance) => this.#run(instance, code));
  }

  /**
   * Execute Python code, journaling callback results so a later run can replay
   * them instead of re-invoking the callbacks.
   *
   * Callbacks matching an entry of `options.journal` (by name and canonical
   * arguments, FIFO) return the recorded result without calling the handler;
   * the first miss switches the rest of the run to live calls. A handler can
   * throw SuspendCallback to halt execution; the outcome then has `suspended`
   * set and a journal to resume from.
   *
   * Never throws for script failures: check `suspended`, then `error`. Like
   * every call on this Sandbox, it waits for earlier calls to finish first.
   *
   * @param {string} code - Python source code to execute
   * @param {{journal?: string}} [options] - A journal from a previous run to replay
   * @returns {Promise<ReplayOutcome>}
   */
  async executeWithJournal(code, options) {
    return this.#serialized(async (instance) => {
      this.#host.beginReplay(options?.journal);
      let result;
      let error;
      try {
        result = await this.#run(instance, code);
      } catch (e) {
        error = e;
      }
      return { result, error, ...this.#host.endReplay(code) };
    });
  }

  /**
   * Set the name of the variable captured as the structured result.
   *
   * After each execute(), the variable with this name is read from the
   * script's namespace, JSON-serialized, and returned as
   * `ExecuteResult.result`. Defaults to "result". Takes effect for calls made
   * after it.
   *
   * @param {string} name - The variable name to capture
   * @returns {Promise<void>}
   */
  async setResultVariable(name) {
    await this.#serialized((instance) => instance.setResultVariable(name));
  }

  /**
   * Capture a snapshot of the current Python session state.
   *
   * Returns serialized state (via pickle) that can be restored later
   * with restoreState(). This captures all user-defined variables.
   *
   * @returns {Promise<Uint8Array>} Serialized Python state
   * @throws {Error} If serialization fails (e.g., unpicklable objects)
   */
  async snapshotState() {
    return this.#serialized((instance) => instance.snapshotState());
  }

  /**
   * Restore Python session state from a previously captured snapshot.
   *
   * After restore, subsequent execute() calls will have access to all
   * variables that were present when the snapshot was taken.
   *
   * @param {Uint8Array} data - Serialized state from snapshotState()
   * @throws {Error} If deserialization fails
   */
  async restoreState(data) {
    await this.#serialized((instance) => instance.restoreState(data));
  }

  /**
   * Clear all persistent state from the session.
   *
   * After clear, subsequent execute() calls will start with a fresh
   * namespace (no user-defined variables from previous calls).
   */
  async clearState() {
    await this.#serialized((instance) => instance.clearState());
  }
}

/** @type {Sandbox | undefined} */
let _defaultSandbox;

/** The sandbox behind the module-level helpers, created on first use. */
function _default() {
  return (_defaultSandbox ??= new Sandbox());
}

/**
 * Execute Python code in the default sandbox.
 *
 * This is a convenience function backed by one module-wide Sandbox, created
 * on first use, so state from previous execute() calls persists. It shares
 * nothing with Sandbox instances you create yourself; use one of those for
 * isolated execution.
 *
 * @param {string} code - Python source code to execute
 * @returns {Promise<ExecuteResult>} Captured stdout and stderr
 * @throws {Error} If the Python code raises an unhandled exception
 *
 * @example
 * const result = await execute('print("hello")');
 * console.log(result.stdout);  // "hello"
 */
export async function execute(code) {
  return _default().execute(code);
}

/**
 * Execute Python code in the default sandbox, journaling callback results.
 * See {@link Sandbox#executeWithJournal}.
 *
 * @param {string} code - Python source code to execute
 * @param {{journal?: string}} [options] - A journal from a previous run to replay
 * @returns {Promise<ReplayOutcome>}
 */
export async function executeWithJournal(code, options) {
  return _default().executeWithJournal(code, options);
}

/**
 * Set the name of the variable captured as the structured result in the
 * default sandbox. See {@link Sandbox#setResultVariable}.
 *
 * @param {string} name - The variable name to capture
 * @returns {Promise<void>}
 */
export function setResultVariable(name) {
  return _default().setResultVariable(name);
}
