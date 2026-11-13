/**
 * What `@eryx-org/eryx` (sandboxes in your own process) shares with this
 * client: the errors a run throws, its result, and callbacks. No `fetch`,
 * `WebSocket` or `node:` imports, so it loads in a browser.
 *
 * @module
 */

export {
  EryxError,
  ExecutionError,
  PythonExceptionError,
  ResourceLimitError,
  TimeoutError,
  type RunStats,
} from "./errors.ts";
export type { CallbackHandler, Callbacks, ExecuteResult } from "./types.ts";
