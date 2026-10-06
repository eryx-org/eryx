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

/**
 * A single recorded callback invocation. Same JSON shape as the Rust
 * `CallbackJournalEntry`.
 */
export interface CallbackJournalEntry {
  /** Position in the invocation sequence (0-indexed, in dispatch order). */
  index: number;
  /** Callback name. */
  name: string;
  /** FNV-1a hash of `args_json` (informational; may be imprecise above 2^53). */
  args_hash: number;
  /** Canonical arguments JSON (object keys sorted). */
  args_json: string;
  /** The recorded success value, or the error message Python observed. */
  result: { Ok: unknown } | { Err: string };
}

/**
 * The callbacks completed during one execution. Plain JSON: persist it with
 * `JSON.stringify` and pass it back via `executeWithJournal(code, { journal })`.
 * Same shape as the Rust `CallbackJournal`.
 */
export interface CallbackJournal {
  /** The script that produced this journal. */
  code: string;
  /** Recorded invocations, in dispatch order. */
  entries: CallbackJournalEntry[];
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
  /** A journal from a previous run whose results should be replayed. */
  journal?: CallbackJournal;
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
  /** Callbacks completed during this run. Always present, even on error. */
  journal: CallbackJournal;
  /** How many callbacks were served from the supplied journal. */
  replayedCallbacks: number;
  /** Set if a callback threw `SuspendCallback`. */
  suspended?: SuspendedCallback;
}

/**
 * A Python sandbox powered by WebAssembly.
 *
 * The sandbox executes Python code in complete isolation. Each Sandbox
 * instance maintains its own Python state (variables, imports, etc.)
 * across multiple execute() calls.
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
   * Execute Python code in the sandbox.
   *
   * The code runs in the sandboxed Python interpreter. Output to stdout
   * and stderr is captured and returned. Variables and imports persist
   * across calls on the same Sandbox instance.
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
   * Never rejects for script failures: check `suspended`, then `error`.
   */
  executeWithJournal(
    code: string,
    options?: ExecuteWithJournalOptions,
  ): Promise<ReplayOutcome>;

  /**
   * Capture a snapshot of the current Python session state.
   *
   * Returns serialized state (via pickle) that can be restored later
   * with restoreState(). This captures all user-defined variables.
   *
   * @returns Serialized Python state
   * @throws If serialization fails (e.g., unpicklable objects)
   */
  snapshotState(): Promise<Uint8Array>;

  /**
   * Restore Python session state from a previously captured snapshot.
   *
   * @param data - Serialized state from snapshotState()
   * @throws If deserialization fails
   */
  restoreState(data: Uint8Array): Promise<void>;

  /**
   * Clear all persistent state from the session.
   */
  clearState(): Promise<void>;
}

/**
 * Execute Python code using the shared global sandbox state.
 *
 * This is a convenience function. For isolated execution, create a
 * Sandbox instance instead.
 *
 * @param code - Python source code to execute
 * @returns Captured stdout and stderr
 * @throws If the Python code raises an unhandled exception
 */
export function execute(code: string): Promise<ExecuteResult>;

/**
 * Execute Python code using the shared global sandbox state, journaling
 * callback results. See {@link Sandbox.executeWithJournal}.
 */
export function executeWithJournal(
  code: string,
  options?: ExecuteWithJournalOptions,
): Promise<ReplayOutcome>;

/**
 * Set the name of the variable captured as the structured result.
 *
 * After each `execute()`, the variable with this name is read from the script's
 * namespace, JSON-serialized, and returned as `ExecuteResult.result`. Applies to
 * the shared sandbox instance. Defaults to `"result"`.
 *
 * The underlying export is async; await the returned promise before calling
 * `execute()` if you need the new name to take effect for the next execution.
 *
 * @param name - The variable name to capture
 */
export function setResultVariable(name: string): Promise<void>;

/**
 * The virtual file tree backing the WASI filesystem.
 *
 * This is the same object passed to `_setFileData()` from the preview2-shim.
 * It contains `python-stdlib` and `site-packages` directories. You can add
 * files to `_fileTree.dir["site-packages"]` and then call `_setFileData(_fileTree)`
 * to make them visible to the sandbox.
 *
 * @internal This is primarily for use by the demo app and advanced integrations.
 */
export const _fileTree: {
  dir: Record<string, unknown>;
};
