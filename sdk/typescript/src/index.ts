/**
 * Run Python in an eryx sandbox on eryx.run.
 *
 * @example
 * import { Sandbox } from "@eryx-org/eryx-run";
 *
 * const sandbox = new Sandbox(); // uses $ERYX_API_KEY
 * const result = await sandbox.execute("print(2 + 2)");
 * console.log(result.stdout); // "4\n"
 *
 * Callbacks are functions the code can `await`, which run here, in your
 * process. So do `volumes`: their directories are served from here,
 * read-only, as the code reads them.
 *
 * @module
 */

export {
  DEFAULT_URL,
  PROTOCOL,
  type CallbackHandler,
  type Callbacks,
  type ConnectionOptions,
  type ExecuteResult,
  type WebSocketConstructor,
} from "./client.ts";
export {
  APIConnectionError,
  APIError,
  AuthenticationError,
  ConflictError,
  EryxError,
  ExecutionError,
  InvalidRequestError,
  NotFoundError,
  PythonExceptionError,
  RateLimitError,
  ResourceLimitError,
  ServiceUnavailableError,
  TimeoutError,
  type RunStats,
} from "./errors.ts";
export {
  Sandbox,
  Session,
  type ExecuteOptions,
  type ResourceLimits,
  type SandboxOptions,
  type SessionOptions,
} from "./sandbox.ts";
export { DEFAULT_HIDDEN, type Volume } from "./volumes.ts";
