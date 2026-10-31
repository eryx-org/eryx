/**
 * Eryx - JavaScript API
 *
 * A Python sandbox powered by WebAssembly, for browser and Node.js.
 */

export {
  setCallbackHandler,
  setCallbacks,
  setTraceHandler,
  setOutputHandler,
  SuspendCallback,
} from "./shims/callbacks.js";

export { DEFAULT_BLOCKED_HOSTS, DEFAULT_BLOCKED_NETWORKS } from "./shims/net.js";
export type { NetConfig } from "./shims/net.js";

import type { CallbackInfo } from "./shims/callbacks.js";
import type { NetConfig } from "./shims/net.js";

/**
 * Set the default network policy, or disable networking again with `null`
 * (the initial default). Each {@link Sandbox} copies it when it is created,
 * unless given its own `netConfig`; sandboxes that already exist keep their
 * policy. It also applies to the default sandbox behind the module-level
 * helpers. Throws a TypeError if a network isn't a valid CIDR or
 * `maxConnections` isn't a non-negative integer.
 */
export function setNetConfig(config: NetConfig | null): void;

/**
 * A captured Python session state, from {@link Sandbox.snapshotState}.
 */
export interface StateSnapshot {
  /** The serialized state, accepted by `restoreState()`. */
  data: Uint8Array;
  /**
   * Names of the globals left out because they could not be serialized (e.g.
   * open sockets, generators). They raise `NameError` after a restore.
   */
  skippedGlobals: string[];
}

/**
 * Result from executing Python code in the sandbox.
 */
export interface ExecuteResult {
  /** Captured standard output */
  stdout: string;
  /** Captured standard error */
  stderr: string;
  /**
   * The script's `result` variable, parsed from JSON into a native value, or
   * `undefined` if it was not set. The captured variable name defaults to
   * `result` and can be changed via {@link setResultVariable}.
   *
   * Integers outside the IEEE-754 safe range (|n| > 2^53 - 1) are parsed as
   * `bigint` to avoid precision loss, so a numeric field may be `number` or
   * `bigint` depending on its magnitude. Use {@link resultJson} if you want the
   * exact text and to control parsing yourself.
   */
  result?: unknown;
  /**
   * The raw JSON string of the captured `result`, or `undefined` if it was not
   * set. Exact (no precision loss); parse it yourself if you need full control.
   */
  resultJson?: string;
  /**
   * Why result capture failed (e.g. the value was not JSON-serializable), or
   * `undefined` when capture succeeded or no result variable was set.
   */
  resultError?: string;
}

/** The callback that suspended execution. */
export interface SuspendedCallback {
  /** Name of the callback that suspended. */
  name: string;
  /** Canonical arguments JSON it was invoked with. */
  argsJson: string;
  /** The reason passed to `SuspendCallback`. */
  reason: string;
}

/** Options for {@link Sandbox.executeWithJournal}. */
export interface ExecuteWithJournalOptions {
  /**
   * The `journal` string from a previous run's outcome, whose results should be
   * replayed. Pass it back unmodified (don't round-trip it through
   * `JSON.parse`/`JSON.stringify`, which would lose number precision in results).
   */
  journal?: string;
}

/** The outcome of {@link Sandbox.executeWithJournal}. */
export interface ReplayOutcome {
  /** The execution result, or `undefined` if execution failed. */
  result?: ExecuteResult;
  /**
   * Why execution failed — a Python exception, or the `SuspendCallback` that
   * halted it — or `undefined` on success. Check `suspended` first.
   */
  error?: Error;
  /**
   * Callbacks completed during this run, serialized as JSON in the same shape as
   * the Rust `CallbackJournal`
   * (`{"code", "entries": [{"index", "name", "args_hash", "args_json", "result": {"Ok": value} | {"Err": message}}]}`).
   * Always present, even on error. Store it as-is.
   */
  journal: string;
  /** How many callbacks were served from the supplied journal. */
  replayedCallbacks: number;
  /** Set if a callback threw `SuspendCallback`. */
  suspended?: SuspendedCallback;
}

/**
 * Options for a {@link Sandbox}.
 *
 * Each handler option overrides, for this sandbox only, the module-wide default
 * set by the setter of the same name (`setCallbackHandler()` and so on). An
 * omitted option follows that default, including later changes to it.
 */
export interface SandboxOptions {
  /** Handler for callbacks invoked by sandbox code. */
  callbackHandler?:
    | ((name: string, argsJson: string) => string | Promise<string>)
    | null;
  /** Callbacks visible to sandbox code via `list_callbacks()`. */
  callbacks?: CallbackInfo[];
  /** Handler for trace events from the Python runtime. */
  traceHandler?:
    | ((lineno: number, eventJson: string, contextJson: string) => void)
    | null;
  /** Handler for streaming output: stream 0 = stdout, 1 = stderr. */
  outputHandler?: ((stream: number, data: string) => void) | null;
  /**
   * This sandbox's network policy (`null` disables networking). Omitted, the
   * sandbox copies the default set with {@link setNetConfig} when it is
   * created; later {@link setNetConfig} calls don't change it.
   */
  netConfig?: NetConfig | null;
  /**
   * Run the sandbox in its own Worker (a `worker_threads` Worker in Node.js)
   * and reject any call that takes longer than this many milliseconds with a
   * {@link TimeoutError}. The Worker is then terminated and replaced, so the
   * sandbox's Python state is lost. The deadline is wall-clock time and
   * includes time spent in callbacks; instantiation doesn't count.
   *
   * Each such sandbox also holds a Worker with its own JavaScript heap and its
   * own copy of the Python stdlib tree, about 70 MB on top of the instance's
   * memory.
   *
   * Opt-in: there is no default timeout, unlike the Rust crate and the Python
   * bindings (30 seconds). Without `timeoutMs`, nothing interrupts a call that
   * never finishes, such as `while True: pass`. There is no memory limit
   * either way: an instance's memory grows as its code allocates, up to the
   * WebAssembly 4 GiB maximum or what the JavaScript engine allows.
   */
  timeoutMs?: number;
}

/** Rejects a call that ran past its Sandbox's `timeoutMs`. */
export class TimeoutError extends Error {
  constructor(timeoutMs: number);
}

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
 * 70 MB. Call close() (or use `using`) when you're done with a sandbox: it
 * stops the Worker at once and closes the sandbox's connections. A sandbox
 * that is only dropped is freed whenever the garbage collector gets to it.
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
  /**
   * Create a sandbox. Instantiation starts right away and finishes in the
   * background; the first call waits for it, and rejects if it failed.
   */
  constructor(options?: SandboxOptions);

  /**
   * This sandbox's own file tree (a copy of the module's `_fileTree`). Changes
   * to it are visible to the sandbox's next filesystem access. Undefined for a
   * sandbox with `timeoutMs`, whose files live in its Worker.
   *
   * @internal For the demo app and advanced integrations.
   */
  readonly _fileTree: { dir: Record<string, unknown> } | undefined;

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
   * @param code - Python source code to execute
   * @returns Captured stdout and stderr
   * @throws If the Python code raises an unhandled exception
   */
  execute(code: string): Promise<ExecuteResult>;

  /**
   * Execute Python code, journaling callback results so a later run can replay
   * them instead of re-invoking the callbacks.
   *
   * Callbacks matching an entry of `options.journal` (by name and canonical
   * arguments, FIFO) return the recorded result without calling the handler;
   * the first miss switches the rest of the run to live calls. A handler can
   * throw {@link SuspendCallback} to halt execution.
   *
   * Never rejects for script failures: check `suspended`, then `error`. Like
   * every call on this Sandbox, it waits for earlier calls to finish first.
   */
  executeWithJournal(
    code: string,
    options?: ExecuteWithJournalOptions,
  ): Promise<ReplayOutcome>;

  /**
   * Set the name of the variable captured as the structured result.
   *
   * After each `execute()`, the variable with this name is read from the
   * script's namespace, JSON-serialized, and returned as
   * `ExecuteResult.result`. Defaults to `"result"`. Takes effect for calls made
   * after it.
   *
   * @param name - The variable name to capture
   */
  setResultVariable(name: string): Promise<void>;

  /**
   * Replace this sandbox's network policy (`null` disables networking). It
   * applies to connections opened after the call, and doesn't change the
   * default set with the module-level {@link setNetConfig} or other
   * sandboxes. Throws a TypeError if a network isn't a valid CIDR or
   * `maxConnections` isn't a non-negative integer.
   */
  setNetConfig(config: NetConfig | null): void;

  /**
   * Stop using this sandbox. Its open connections are closed, and every later
   * call rejects. For a `timeoutMs` sandbox the Worker is terminated at once,
   * so a call in flight rejects too; without `timeoutMs` a call in flight runs
   * to the end. Calling it again does nothing. `using` and `await using` call
   * it at the end of their scope.
   */
  close(): void;
  [Symbol.dispose](): void;
  [Symbol.asyncDispose](): void;

  /**
   * Capture a snapshot of the current Python session state.
   *
   * Returns serialized state (via pickle) that can be restored later
   * with restoreState(). This captures all user-defined variables that can
   * be serialized, and names the others in `skippedGlobals`.
   *
   * @returns Serialized Python state
   * @throws If serialization fails
   */
  snapshotState(): Promise<StateSnapshot>;

  /**
   * Restore Python session state from a previously captured snapshot.
   *
   * @param snapshot - A snapshot from snapshotState(), or its `data`
   * @throws If deserialization fails
   */
  restoreState(snapshot: StateSnapshot | Uint8Array): Promise<void>;

  /**
   * Clear all persistent state from the session.
   */
  clearState(): Promise<void>;
}

/**
 * Execute Python code in the default sandbox.
 *
 * This is a convenience function backed by one module-wide Sandbox, created on
 * first use, so state from previous calls persists. It shares nothing with
 * Sandbox instances you create yourself; use one of those for isolated
 * execution.
 *
 * @param code - Python source code to execute
 * @returns Captured stdout and stderr
 * @throws If the Python code raises an unhandled exception
 */
export function execute(code: string): Promise<ExecuteResult>;

/**
 * Execute Python code in the default sandbox, journaling callback results.
 * See {@link Sandbox.executeWithJournal}.
 */
export function executeWithJournal(
  code: string,
  options?: ExecuteWithJournalOptions,
): Promise<ReplayOutcome>;

/**
 * Set the name of the variable captured as the structured result in the
 * default sandbox (the one behind {@link execute}). See
 * {@link Sandbox.setResultVariable}.
 *
 * @param name - The variable name to capture
 */
export function setResultVariable(name: string): Promise<void>;

/**
 * The template file tree each new Sandbox copies: `python-stdlib` and an empty
 * `site-packages` directory. Files added here are visible to sandboxes created
 * afterwards; change {@link Sandbox._fileTree} to reach one that exists.
 *
 * @internal This is primarily for use by the demo app and advanced integrations.
 */
export const _fileTree: {
  dir: Record<string, unknown>;
};
