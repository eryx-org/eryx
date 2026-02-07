/**
 * Callback shims for the eryx sandbox.
 *
 * These provide the host-side implementations of the sandbox's callback imports:
 * - invoke: call a registered callback by name with JSON arguments
 * - listCallbacks: list all registered callbacks
 * - reportTrace: receive trace events from the Python runtime
 *
 * Users can register callbacks via setCallbackHandler() and setTraceHandler().
 */

/** @type {((name: string, argsJson: string) => string | Promise<string>) | null} */
let _callbackHandler = null;

/** @type {((lineno: number, eventJson: string, contextJson: string) => void) | null} */
let _traceHandler = null;

/** @type {Array<{name: string, description: string, parametersSchemaJson: string}>} */
let _registeredCallbacks = [];

/**
 * Set the callback handler for sandbox code to invoke.
 *
 * The handler receives a callback name and JSON-encoded arguments,
 * and should return a JSON-encoded result (or a Promise of one).
 *
 * @param {((name: string, argsJson: string) => string | Promise<string>) | null} handler
 */
export function setCallbackHandler(handler) {
  _callbackHandler = handler;
}

/**
 * Register callbacks that will be visible to sandbox code via list_callbacks().
 *
 * @param {Array<{name: string, description: string, parametersSchemaJson?: string}>} callbacks
 */
export function setCallbacks(callbacks) {
  _registeredCallbacks = callbacks.map((cb) => ({
    name: cb.name,
    description: cb.description,
    parametersSchemaJson: cb.parametersSchemaJson ?? "{}",
  }));
}

/**
 * Set a handler for trace events from the Python runtime.
 *
 * @param {((lineno: number, eventJson: string, contextJson: string) => void) | null} handler
 */
export function setTraceHandler(handler) {
  _traceHandler = handler;
}

/**
 * Invoke a callback by name with JSON arguments.
 * This is called by the sandbox runtime when Python code calls invoke().
 *
 * @param {string} name - Callback name
 * @param {string} argumentsJson - JSON-encoded arguments
 * @returns {string} JSON-encoded result
 */
export function invoke(name, argumentsJson) {
  if (!_callbackHandler) {
    throw new Error(
      `No callback handler registered. Call setCallbackHandler() before executing code that uses callbacks. Attempted to invoke: ${name}`,
    );
  }
  return _callbackHandler(name, argumentsJson);
}

/**
 * List all registered callbacks.
 * This is called by the sandbox runtime when Python code calls list_callbacks().
 *
 * @returns {Array<{name: string, description: string, parametersSchemaJson: string}>}
 */
export function listCallbacks() {
  return _registeredCallbacks;
}

/**
 * Report a trace event from the Python runtime.
 * This is called by the sandbox runtime's sys.settrace hook.
 *
 * @param {number} lineno - Line number
 * @param {string} eventJson - Event type as JSON
 * @param {string} contextJson - Context data as JSON
 */
export function reportTrace(lineno, eventJson, contextJson) {
  if (_traceHandler) {
    _traceHandler(lineno, eventJson, contextJson);
  }
}
