/**
 * Eryx - JavaScript API
 *
 * A Python sandbox powered by WebAssembly, for browser and Node.js.
 */

export {
  setCallbackHandler,
  setCallbacks,
  setTraceHandler,
} from "./shims/callbacks.js";

/**
 * Result from executing Python code in the sandbox.
 */
export interface ExecuteResult {
  /** Captured standard output */
  stdout: string;
  /** Captured standard error */
  stderr: string;
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
