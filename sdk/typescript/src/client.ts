/**
 * The hosted API: HTTP for plain runs and sessions, `/v1/stream` for runs
 * with callbacks or volumes.
 *
 * @module
 */

import {
  APIConnectionError,
  APIError,
  EryxError,
  ExecutionError,
  PythonExceptionError,
  RateLimitError,
  ResourceLimitError,
  TimeoutError,
  apiError,
} from "./errors.ts";
import type { Volume } from "./volumes.ts";

export const DEFAULT_URL = "https://api.eryx.run";

/** The `/v1/stream` protocol version this client speaks */
export const PROTOCOL = "eryx.v1";

/**
 * The result of a run, with the same fields as `@bsull/eryx`'s
 * `ExecuteResult` plus the run's stats and a few only eryx.run has
 * (`networkBytes`, `traceId`, `unsavedVariables`).
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
   * Whether a session lost its state before this run. eryx.run sessions never
   * do: each run restores the saved state into a fresh sandbox.
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

/** A `WebSocket` constructor that takes headers, as Node.js's, Bun's and `ws`'s do. */
export type WebSocketConstructor = new (
  url: string,
  options: { protocols: string[]; headers: Record<string, string> },
) => WebSocket;

/** Where to send requests, and the key to send with them. */
export interface ConnectionOptions {
  /** The API key. Default: `$ERYX_API_KEY`. */
  apiKey?: string;
  /** The API's URL. Default: `$ERYX_URL`, else `https://api.eryx.run`. */
  baseUrl?: string;
  /** The `fetch` to use. Default: the global one. */
  fetch?: typeof fetch;
  /**
   * The `WebSocket` to use for runs with callbacks or volumes. It must take
   * an `Authorization` header, so the global one works in Node.js 22+ and
   * Bun; elsewhere pass e.g. the `ws` package's.
   */
  WebSocket?: WebSocketConstructor;
}

type Json = Record<string, unknown>;

function env(name: string): string | undefined {
  return (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env[name];
}

export class Connection {
  readonly baseUrl: string;
  private readonly apiKey: string;
  private readonly fetch: typeof fetch;
  private readonly WebSocket: WebSocketConstructor;

  constructor(options: ConnectionOptions) {
    const apiKey = options.apiKey ?? env("ERYX_API_KEY");
    if (!apiKey) {
      throw new TypeError("No API key: pass apiKey or set ERYX_API_KEY (create one at https://app.eryx.run)");
    }
    this.apiKey = apiKey;
    this.baseUrl = (options.baseUrl ?? env("ERYX_URL") ?? DEFAULT_URL).replace(/\/+$/, "");
    this.fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.WebSocket = options.WebSocket ?? (globalThis.WebSocket as unknown as WebSocketConstructor);
  }

  private get headers(): Record<string, string> {
    return { Authorization: `Bearer ${this.apiKey}` };
  }

  /** Send a request and return its JSON body, throwing `APIError` for an error response. */
  async request(method: string, path: string, body?: Json, signal?: AbortSignal): Promise<Json | undefined> {
    let response: Response;
    try {
      response = await this.fetch(this.baseUrl + path, {
        method,
        headers: body === undefined ? this.headers : { ...this.headers, "Content-Type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal,
      });
    } catch (e) {
      if (signal?.aborted) throw signal.reason;
      throw new APIConnectionError(`Couldn't reach ${this.baseUrl}: ${e}`, { cause: e });
    }
    if (!response.ok) throw await errorResponse(response);
    const text = await response.text();
    return text ? (JSON.parse(text) as Json) : undefined;
  }

  /**
   * Run `body` (an `execute` message) over `/v1/stream`, answering the
   * code's callbacks and file operations on `volumes`, and return the
   * `completed` message. Aborting closes the socket; the run carries on on
   * eryx.run until it finishes or hits a limit.
   */
  async stream(
    body: Json,
    callbacks: Callbacks,
    volumes: readonly Volume[],
    hidden: readonly string[],
    signal?: AbortSignal,
  ): Promise<Json> {
    signal?.throwIfAborted();
    const fs = volumes.length ? await import("./mounts.ts") : undefined;
    const mounts = fs ? await fs.mounts(volumes, hidden) : new Map();
    const message = {
      type: "execute",
      ...body,
      callbacks: Object.entries(callbacks).map(([name, cb]) => ({
        name,
        description: typeof cb === "function" ? "" : (cb.description ?? ""),
      })),
      ...(volumes.length ? { mounts: volumes.map((v) => ({ path: v.guestPath })) } : {}),
    };
    const url = `ws${this.baseUrl.replace(/^http/, "")}/v1/stream`;
    const ws = new this.WebSocket(url, { protocols: [PROTOCOL], headers: this.headers });
    ws.binaryType = "arraybuffer";

    return new Promise<Json>((resolve, reject) => {
      let opened = false;
      let done = false;
      const end = (settle: () => void) => {
        if (done) return;
        done = true;
        signal?.removeEventListener("abort", onAbort);
        ws.close();
        settle();
      };
      const onAbort = () => end(() => reject(signal!.reason));
      signal?.addEventListener("abort", onAbort);

      const send = (data: string | Uint8Array<ArrayBuffer>) => {
        if (!done) ws.send(data);
      };

      const answer = async (call: Json) => {
        const reply = { type: "callback_response", call_id: call.call_id };
        const cb = callbacks[String(call.name)];
        let text: string;
        // A result JSON can't hold is an error too, so the code isn't left
        // waiting for an answer
        try {
          if (!cb || !Object.hasOwn(callbacks, String(call.name))) throw new Error(`no callback ${call.name}`);
          const handler = typeof cb === "function" ? cb : cb.handler;
          const result = await handler((call.args as Json | undefined) ?? {});
          text = JSON.stringify({ ...reply, result: result ?? null });
        } catch (e) {
          // The code sees it as a RuntimeError
          const error = e instanceof Error ? `${e.name}: ${e.message}` : String(e);
          text = JSON.stringify({ ...reply, error });
        }
        send(text);
      };

      const answerFile = async (op: Json) => {
        const [reply, data] = await fs!.answer(mounts, op);
        // The data must come straight after its reply: sends are queued in order
        send(JSON.stringify(reply));
        if (data) send(data as Uint8Array<ArrayBuffer>);
      };

      ws.onopen = () => {
        opened = true;
        ws.send(JSON.stringify(message));
      };
      ws.onmessage = (event: MessageEvent) => {
        if (typeof event.data !== "string") return;
        let msg: Json;
        try {
          msg = JSON.parse(event.data) as Json;
        } catch (e) {
          end(() => reject(new APIConnectionError(`eryx.run sent a message that isn't JSON: ${e}`, { cause: e })));
          return;
        }
        switch (msg.type) {
          case "callback":
            void answer(msg);
            break;
          case "fs":
            if (fs) void answerFile(msg);
            break;
          case "completed":
            end(() => resolve(msg));
            break;
          case "error":
            end(() => reject(apiError(String(msg.message ?? ""), msg.code as string | undefined, undefined)));
            break;
          // Other types are newer additions to the protocol: ignore them
        }
      };
      // Node.js 22 fires only `error` when the connection fails, so either ends the run
      const onFail = (event: Event) => {
        if (done) return;
        if (opened) {
          const code = "code" in event ? ` (${event.code})` : "";
          end(() => reject(new APIConnectionError(`The connection closed before the run completed${code}`)));
          return;
        }
        // The upgrade was refused, and WebSocket doesn't say why
        end(() => {
          this.refusal(signal).then(reject, reject);
        });
      };
      ws.onerror = onFail;
      ws.onclose = onFail;
    });
  }

  /**
   * Why `/v1/stream` refused to upgrade. WebSocket doesn't give the
   * response, so ask again over HTTP: a refused key or rate limit answers
   * the same way.
   */
  // ponytail: an extra request per failed connection; a WebSocket that
  // exposed the handshake response would make it unnecessary
  private async refusal(signal?: AbortSignal): Promise<EryxError> {
    const why = `Couldn't connect to ${this.baseUrl}/v1/stream`;
    try {
      const response = await this.fetch(`${this.baseUrl}/v1/stream`, { headers: this.headers, signal });
      if (response.status === 401 || response.status === 403 || response.status === 429) {
        return await errorResponse(response);
      }
    } catch (e) {
      if (signal?.aborted) return signal.reason;
      return new APIConnectionError(`${why}: ${e}`, { cause: e });
    }
    return new APIConnectionError(why);
  }
}

/** The error for an error response */
async function errorResponse(response: Response): Promise<APIError> {
  let message = `HTTP ${response.status}`;
  let code: string | undefined;
  try {
    const { error } = (await response.json()) as { error: { code: string; message: string } };
    if (typeof error.message === "string" && typeof error.code === "string") ({ message, code } = error);
  } catch {
    // Not the API's error body
  }
  const error = apiError(message, code, response.status);
  const retryAfter = Number.parseFloat(response.headers.get("Retry-After") ?? "");
  if (error instanceof RateLimitError && !Number.isNaN(retryAfter)) error.retryAfter = retryAfter;
  return error;
}

/** The result of a finished run, or the error the Python SDK raises for it */
export function finish(response: Json): ExecuteResult {
  const stdout = String(response.stdout ?? "");
  const stderr = String(response.stderr ?? "");
  const stats = {
    durationMs: Number(response.duration_ms ?? 0),
    callbackInvocations: Number(response.callback_invocations ?? 0),
    peakMemoryBytes: response.peak_memory_bytes as number | undefined,
    fuelConsumed: response.compute_units as number | undefined,
  };
  const status = response.status;
  if (status === "success") {
    const result: ExecuteResult = {
      stdout,
      stderr,
      stateWasReset: false,
      unsavedVariables: (response.unsaved_variables as string[] | undefined) ?? [],
      ...stats,
    };
    if ("result" in response) {
      result.result = response.result;
      result.resultJson = JSON.stringify(response.result);
    }
    if (response.result_error != null) result.resultError = String(response.result_error);
    if (response.network_bytes != null) result.networkBytes = Number(response.network_bytes);
    if (response.trace_id != null) result.traceId = String(response.trace_id);
    return result;
  }
  const message = String(response.error || `run ended with status ${JSON.stringify(status)}`);
  let error: EryxError;
  if (status === "timeout") {
    error = new TimeoutError(message);
  } else if (status === "resource_limit") {
    error = new ResourceLimitError(message);
  } else if (message.startsWith("Traceback (most recent call last)")) {
    // ponytail: the API says `error` for both an exception in the code and a
    // failure around it, whose message is the traceback only for the former;
    // use a failure kind once the API sends one
    const raised = new PythonExceptionError(message);
    raised.stdout = stdout;
    raised.stderr = stderr;
    error = raised;
  } else {
    error = new ExecutionError(message);
  }
  throw Object.assign(error, stats);
}
