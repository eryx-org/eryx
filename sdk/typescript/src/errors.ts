/**
 * Errors, with the same names and hierarchy as the Python SDK's.
 *
 * A failed run's error carries what it used, as on `ExecuteResult`
 * (`durationMs`, `callbackInvocations`, `peakMemoryBytes`, `fuelConsumed`;
 * `undefined` when unknown). The `APIError` group is for requests eryx.run
 * refused.
 *
 * @module
 */

/** What a run used, on its result or on the error it failed with. */
export interface RunStats {
  /** How long the run took, in milliseconds. */
  durationMs?: number;
  /** How many callback calls the code made. */
  callbackInvocations?: number;
  /** Peak memory the sandbox used, in bytes. */
  peakMemoryBytes?: number;
  /** Fuel (WebAssembly instructions) the run used: eryx.run's compute units. */
  fuelConsumed?: number;
}

/** Base class for every error this package throws. */
export class EryxError extends Error {
  override name = "EryxError";
  durationMs?: number;
  callbackInvocations?: number;
  peakMemoryBytes?: number;
  fuelConsumed?: number;
}

/** The run failed. */
export class ExecutionError extends EryxError {
  override name = "ExecutionError";
}

/**
 * The code raised an uncaught exception. `message` is its traceback;
 * `stdout` and `stderr` hold what the code wrote before it raised (`stderr`
 * ends with the traceback).
 */
export class PythonExceptionError extends ExecutionError {
  override name = "PythonExceptionError";
  stdout = "";
  stderr = "";
}

/** The run hit its compute unit (fuel) or memory limit. */
export class ResourceLimitError extends EryxError {
  override name = "ResourceLimitError";
}

/** The run hit its time limit. */
export class TimeoutError extends EryxError {
  override name = "TimeoutError";
}

/**
 * eryx.run refused a request.
 *
 * `code` is the API's error code (`"invalid_key"`, `"session_busy"`, ...) and
 * `status` the HTTP status. Errors sent over the stream have no status.
 */
export class APIError extends EryxError {
  override name = "APIError";
  readonly code: string | undefined;
  readonly status: number | undefined;

  constructor(message: string, options: { code?: string | undefined; status?: number | undefined; cause?: unknown } = {}) {
    super(message, { cause: options.cause });
    this.code = options.code;
    this.status = options.status;
  }
}

/** eryx.run couldn't be reached, or the connection dropped mid-run. */
export class APIConnectionError extends APIError {
  override name = "APIConnectionError";
}

/** The API key is missing, malformed, wrong or revoked. */
export class AuthenticationError extends APIError {
  override name = "AuthenticationError";
}

/** The request wasn't valid. */
export class InvalidRequestError extends APIError {
  override name = "InvalidRequestError";
}

/** No such session, environment, connection or network policy. */
export class NotFoundError extends APIError {
  override name = "NotFoundError";
}

/** The session is busy, or the environment isn't ready or already exists. */
export class ConflictError extends APIError {
  override name = "ConflictError";
}

/**
 * Over a rate, concurrency or count limit. `retryAfter` is how many seconds
 * to wait, when the API said.
 */
export class RateLimitError extends APIError {
  override name = "RateLimitError";
  retryAfter?: number;
}

/** The service is at capacity, or failed; retry after a short wait. */
export class ServiceUnavailableError extends APIError {
  override name = "ServiceUnavailableError";
}

const API_ERRORS: Record<string, typeof APIError> = {
  missing_auth: AuthenticationError,
  invalid_auth: AuthenticationError,
  invalid_key: AuthenticationError,
  invalid_request: InvalidRequestError,
  not_found: NotFoundError,
  conflict: ConflictError,
  environment_not_ready: ConflictError,
  session_busy: ConflictError,
  rate_limit_exceeded: RateLimitError,
  concurrency_limit: RateLimitError,
  session_limit: RateLimitError,
  environment_limit: RateLimitError,
  connection_limit: RateLimitError,
  network_policy_limit: RateLimitError,
  unavailable: ServiceUnavailableError,
  internal_error: ServiceUnavailableError,
};

/** The error for an API error code. */
export function apiError(message: string, code: string | undefined, status: number | undefined): APIError {
  const Class = (code !== undefined && Object.hasOwn(API_ERRORS, code) && API_ERRORS[code]) || APIError;
  return new Class(message, { code, status });
}
