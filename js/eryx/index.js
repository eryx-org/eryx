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

import { createCallbackHost } from "./shims/callbacks.js";
import { createNet } from "./shims/net.js";
import {
  _fileTree,
  decodeError,
  encodeError,
  forkTree,
  getCoreModule,
  instantiateSandbox,
  isNode,
} from "./instance.js";

export { _fileTree } from "./instance.js";

/** Rejects a call that ran past its Sandbox's `timeoutMs`. */
export class TimeoutError extends Error {
  /** @param {number} timeoutMs */
  constructor(timeoutMs) {
    super(`execution timed out after ${timeoutMs} ms`);
    this.name = "TimeoutError";
  }
}

/**
 * Start a sandbox instance in its own Worker and return a proxy with the
 * instance's methods, plus `terminate()`. The handlers, journal and network
 * stay on this thread; the Worker forwards to them (see worker.js).
 * @param {ReturnType<typeof createCallbackHost>} host
 * @param {ReturnType<typeof createNet>} net - The sandbox's connection table
 */
async function instantiateInWorker(host, net) {
  const url = new URL("./worker.js", import.meta.url);
  const worker = isNode
    ? // Not the parent's execArgv, which can hold `-e`/`--input-type` a Worker
      // rejects. V8 flags such as --experimental-wasm-jspi are process-wide,
      // so the Worker keeps them.
      new (await import(/* webpackIgnore: true */ "node:worker_threads")).Worker(url, {
        execArgv: [],
      })
    : new Worker(url, { type: "module" });
  const targets = {
    module: getCoreModule,
    invoke: host.imports.invoke,
    reportTrace: host.imports.reportTrace,
    reportOutput: host.imports.reportOutput,
    "tcp.connect": net.tcp.connect,
    "tcp.read": net.tcp.read,
    "tcp.write": net.tcp.write,
    "tcp.close": net.tcp.close,
    "tls.upgrade": net.tls.upgrade,
    "tls.read": net.tls.read,
    "tls.write": net.tls.write,
    "tls.close": net.tls.close,
  };

  let nextId = 0;
  const pending = new Map();
  // An idle Worker mustn't keep Node running, but a busy one must.
  const busy = (yes) => isNode && (yes ? worker.ref() : worker.unref());
  let failure = null;
  let ready;
  const started = new Promise((resolve, reject) => (ready = { resolve, reject }));
  // Once the Worker is gone, nothing will close what its code left open.
  const fail = (error) => {
    net.closeConnections();
    failure ??= error;
    ready.reject(failure);
    for (const { reject } of pending.values()) reject(failure);
    pending.clear();
  };

  const onMessage = async (message) => {
    switch (message.type) {
      case "ready":
        busy(false);
        ready.resolve();
        break;
      case "failed":
        fail(decodeError(message));
        break;
      case "result": {
        const call = pending.get(message.id);
        pending.delete(message.id);
        if (pending.size === 0) busy(false);
        if ("value" in message) call?.resolve(message.value);
        else call?.reject(decodeError(message));
        break;
      }
      case "notify":
        targets[message.target](...message.args);
        break;
      case "request":
        try {
          const value = await targets[message.target](...message.args);
          worker.postMessage({ type: "reply", id: message.id, value });
        } catch (e) {
          worker.postMessage({ type: "reply", id: message.id, ...encodeError(e) });
        }
        break;
    }
  };
  const onError = (e) => fail(e instanceof Error ? e : new Error(e.message ?? "sandbox worker failed"));
  if (isNode) {
    worker.on("message", onMessage);
    worker.on("error", onError);
    worker.on("exit", () => fail(new Error("sandbox worker exited")));
  } else {
    worker.addEventListener("message", (e) => onMessage(e.data));
    worker.addEventListener("error", onError);
  }

  const call = (method, ...args) => {
    if (failure) return Promise.reject(failure);
    const id = nextId++;
    busy(true);
    worker.postMessage({
      type: "call",
      id,
      method,
      args,
      callbacks: host.imports.listCallbacks(),
      options: host.imports.getExecutionOptions(),
    });
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
  };

  await started;
  return {
    execute: (code) => call("execute", code),
    snapshotState: () => call("snapshotState"),
    restoreState: (data) => call("restoreState", data),
    clearState: () => call("clearState"),
    setResultVariable: (name) => call("setResultVariable", name),
    /** Stop the Worker, rejecting anything still pending with `error`. */
    terminate(error) {
      fail(error);
      worker.terminate();
    },
  };
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
 * @property {number} [timeoutMs] - Run the sandbox in its own Worker and
 *   reject any call that takes longer than this many milliseconds with a
 *   {@link TimeoutError}. The Worker is then terminated and replaced, so the
 *   sandbox's Python state is lost. The deadline is wall-clock time, including
 *   callbacks; instantiation doesn't count. The Worker costs about 70 MB.
 *   Without it, nothing interrupts a call.
 */

/**
 * A Python sandbox powered by WebAssembly.
 *
 * Each Sandbox is its own WebAssembly instance, with its own Python
 * interpreter, filesystem, network connections, callback handlers and
 * journal, so code run in one can't see or change another's state. Python
 * state (variables, imports, etc.) persists across execute() calls on the same
 * Sandbox; connections are closed when each call finishes.
 *
 * The compiled code is shared, but every instance has its own linear memory
 * (the pre-initialized interpreter image plus whatever the code allocates), so
 * each live Sandbox costs tens of megabytes (about 20 MB idle on Node.js 24).
 * One created with `timeoutMs` also runs in its own Worker, which adds about
 * 70 MB. Drop references to sandboxes you no longer need so they can be
 * garbage collected.
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
  /** @type {ReturnType<typeof createNet>} */
  #net = createNet();
  /** @type {number | undefined} */
  #timeoutMs;
  /** @type {Promise<any>} */
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
    const { timeoutMs } = options;
    if (timeoutMs !== undefined && !(Number.isFinite(timeoutMs) && timeoutMs > 0)) {
      throw new TypeError("timeoutMs must be a positive number of milliseconds");
    }
    this.#timeoutMs = timeoutMs;
    /**
     * This sandbox's own file tree (a copy of the module's `_fileTree`).
     * Changes to it are visible to the sandbox's next filesystem access.
     * Undefined for a sandbox with `timeoutMs`, whose files live in its Worker.
     * @internal For the demo app and advanced integrations.
     */
    this._fileTree = timeoutMs === undefined ? forkTree(_fileTree) : undefined;
    this.#host = createCallbackHost(options);
    this.#start();
  }

  #start() {
    this.#instance =
      this.#timeoutMs === undefined
        ? instantiateSandbox(this._fileTree, this.#host.imports, this.#net)
        : instantiateInWorker(this.#host, this.#net);
    // Surfaced by the first call instead.
    this.#instance.catch(() => {});
  }

  /**
   * Run `fn` on the instance once every call queued before it has settled.
   * @template T
   * @param {(instance: any) => Promise<T>} fn
   * @returns {Promise<T>}
   */
  #serialized(fn) {
    const run = this.#queue.then(() => this.#call(fn));
    this.#queue = run.catch(() => {});
    return run;
  }

  /**
   * Run `fn` on the instance, under the deadline if there is one. On timeout,
   * the Worker is terminated and a fresh one started for later calls.
   * @template T
   * @param {(instance: any) => Promise<T>} fn
   * @returns {Promise<T>}
   */
  async #call(fn) {
    const instance = await this.#instance;
    if (this.#timeoutMs === undefined) return fn(instance);
    // Terminating the Worker rejects the call in flight with the TimeoutError,
    // which executeWithJournal() then reports like any other failure.
    const timer = setTimeout(() => {
      instance.terminate(new TimeoutError(this.#timeoutMs));
      this.#start();
    }, this.#timeoutMs);
    try {
      return await fn(instance);
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * Run code, flushing streamed output when it finishes. Callers must hold the
   * queue, so the flush can't cut into another execution's output.
   * @param {any} instance
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
      // Connections don't outlive the execution that opened them.
      this.#net.closeConnections();
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
