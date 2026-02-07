/**
 * Callback info visible to sandbox code.
 */
export interface CallbackInfo {
  /** Unique name for this callback */
  name: string;
  /** Human-readable description */
  description: string;
  /** JSON Schema for expected arguments */
  parametersSchemaJson?: string;
}

/**
 * Set the callback handler for sandbox code to invoke.
 *
 * The handler receives a callback name and JSON-encoded arguments,
 * and should return a JSON-encoded result (or a Promise of one).
 */
export function setCallbackHandler(
  handler:
    | ((name: string, argsJson: string) => string | Promise<string>)
    | null,
): void;

/**
 * Register callbacks that will be visible to sandbox code via list_callbacks().
 */
export function setCallbacks(callbacks: CallbackInfo[]): void;

/**
 * Set a handler for trace events from the Python runtime.
 */
export function setTraceHandler(
  handler:
    | ((lineno: number, eventJson: string, contextJson: string) => void)
    | null,
): void;

/**
 * Set a handler for streaming output (stdout/stderr) from the Python runtime.
 *
 * The handler is called in real-time as Python code writes to stdout or stderr,
 * rather than waiting for execution to complete.
 *
 * @param handler - stream: 0 = stdout, 1 = stderr; data: the text written
 */
export function setOutputHandler(
  handler: ((stream: number, data: string) => void) | null,
): void;
