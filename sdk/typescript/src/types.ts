/**
 * Types shared by eryx.run's client and the local `@eryx-org/eryx` package,
 * so code written against them runs on either.
 *
 * @module
 */

/**
 * The result of a run, here or in `@eryx-org/eryx`. A few fields only
 * eryx.run has (`networkBytes`, `traceId`, and `unsavedVariables`, which is
 * empty locally), and a local sandbox doesn't report `fuelConsumed` or
 * `peakMemoryBytes`.
 */
export interface ExecuteResult {
  /** What the code printed to stdout. */
  stdout: string;
  /** What the code printed to stderr. */
  stderr: string;
  /** The code's `result` variable, parsed from JSON, or `undefined` if it wasn't set. */
  result?: unknown;
  /** The JSON of the code's `result` variable, or `undefined` if it wasn't set. */
  resultJson?: string;
  /** Why `result` couldn't be captured (e.g. not JSON-serializable). */
  resultError?: string;
  /**
   * Whether the sandbox lost its Python state before this run: in
   * `@eryx-org/eryx`, because an earlier run hit `timeoutMs`. eryx.run
   * sessions never do: each run restores the saved state into a fresh sandbox.
   */
  stateWasReset: boolean;
  /** How long the run took, in milliseconds. */
  durationMs: number;
  /** How many callback calls the code made. */
  callbackInvocations: number;
  /** Peak memory the sandbox used, in bytes. */
  peakMemoryBytes?: number;
  /** Fuel (WebAssembly instructions) the run used: eryx.run's compute units. */
  fuelConsumed?: number;
  /** Bytes the run sent and received over the network. */
  networkBytes?: number;
  /** The run's trace, when it was traced (runs over the stream). */
  traceId?: string;
  /** Session variables that couldn't be saved. */
  unsavedVariables: string[];
}

/**
 * A function sandboxed code can `await`, called with its keyword arguments.
 * It may return a promise; what it returns must be JSON. If it throws, the
 * code gets a `RuntimeError`. (`any`, so a handler can destructure the
 * arguments without declaring their types.)
 */
export type CallbackHandler = (args: Record<string, any>) => unknown;

/**
 * Callbacks by the name the code calls them by: a function, or one with a
 * description, which the code sees in `list_callbacks()`.
 */
export type Callbacks = Record<string, CallbackHandler | { handler: CallbackHandler; description?: string }>;

/** A JSON object, as the API sends it */
export type Json = Record<string, unknown>;
