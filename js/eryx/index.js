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
 *   console.log(result.stdout);  // "hello"
 *   ```
 */

// Import the jco-generated bindings
import {
  execute as _execute,
  snapshotState as _snapshotState,
  restoreState as _restoreState,
  clearState as _clearState,
  finalizePreinit as _finalizePreinit,
  setResultVariable as _setResultVariable,
} from "./eryx-sandbox.js";

// Import filesystem shim to populate with Python stdlib
import { _setFileData } from "@bytecodealliance/preview2-shim/filesystem";

// Import stdlib loader
import { loadStdlib } from "./stdlib-loader.js";

// Load the Python stdlib into the preview2-shim virtual filesystem.
// This must happen before _finalizePreinit() so Python can find stdlib modules.
const stdlibTree = await loadStdlib();

// The stdlib tar extracts to python-stdlib/*, but the WASM expects
// files at the root of the preopen (mounted as /python-stdlib).
// So we set the root fileData to contain the stdlib directory contents.
const stdlibDir = stdlibTree.dir?.["python-stdlib"] || stdlibTree;

// Build the file tree with stdlib and an empty site-packages directory.
// This tree is exported so consumers (e.g., the demo) can add packages to it.
export const _fileTree = {
  dir: { "python-stdlib": stdlibDir, "site-packages": { dir: {} } },
};
_setFileData(_fileTree);

// Complete Python interpreter initialization.
// The pre-initialized WASM has Python's core state baked in, but
// finalizePreinit() must still be called to finish setup.
_finalizePreinit();

// Re-export callback configuration
export {
  setCallbackHandler,
  setCallbacks,
  setTraceHandler,
  setOutputHandler,
} from "./shims/callbacks.js";

/**
 * Result from executing Python code in the sandbox.
 * @typedef {Object} ExecuteResult
 * @property {string} stdout - Captured standard output
 * @property {string} stderr - Captured standard error
 * @property {*} [result] - The script's `result` variable, parsed from JSON, or
 *   undefined if it was not set. See {@link setResultVariable} to change the name.
 * @property {string} [resultError] - Why result capture failed (e.g. the value was
 *   not JSON-serializable), or undefined on success.
 */

/**
 * Map a raw jco execute-output record into the public ExecuteResult shape,
 * parsing the captured result variable from JSON.
 * @param {Object} output
 * @returns {ExecuteResult}
 */
function _toResult(output) {
  return {
    stdout: output.stdout,
    stderr: output.stderr,
    result: output.resultJson ? JSON.parse(output.resultJson) : undefined,
    resultError: output.resultError || undefined,
  };
}

/**
 * Set the name of the variable captured as the structured result.
 *
 * After each execute(), the variable with this name is read from the script's
 * namespace, JSON-serialized, and returned as `ExecuteResult.result`. Applies to
 * the shared sandbox instance. Defaults to "result".
 *
 * @param {string} name - The variable name to capture
 */
export function setResultVariable(name) {
  _setResultVariable(name);
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
   * @param {string} code - Python source code to execute
   * @returns {Promise<ExecuteResult>} Captured stdout and stderr
   * @throws {Error} If the Python code raises an unhandled exception
   *
   * @example
   * const result = await sandbox.execute('print("hello")');
   * console.log(result.stdout);  // "hello"
   */
  async execute(code) {
    const output = await _execute(code);
    if (output.tag === "error") {
      throw new Error(output.val);
    }
    return _toResult(output);
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
    return _snapshotState();
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
    await _restoreState(data);
  }

  /**
   * Clear all persistent state from the session.
   *
   * After clear, subsequent execute() calls will start with a fresh
   * namespace (no user-defined variables from previous calls).
   */
  async clearState() {
    await _clearState();
  }
}

/**
 * Execute Python code in a fresh sandbox.
 *
 * This is a convenience function that uses the shared global sandbox state.
 * For isolated execution, create a Sandbox instance instead.
 *
 * Note: Unlike the Sandbox class, state from previous execute() calls
 * persists because this uses the same underlying WASM instance. Call
 * clearState() on a Sandbox instance to reset.
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
  const output = await _execute(code);
  if (output.tag === "error") {
    throw new Error(output.val);
  }
  return _toResult(output);
}
