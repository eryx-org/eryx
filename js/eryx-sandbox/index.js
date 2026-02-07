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
_setFileData({ dir: { "python-stdlib": stdlibDir } });

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
 */

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
    return {
      stdout: output.stdout,
      stderr: output.stderr,
    };
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
  return {
    stdout: output.stdout,
    stderr: output.stderr,
  };
}
