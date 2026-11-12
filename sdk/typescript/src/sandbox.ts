/**
 * `Sandbox` and `Session`, running on eryx.run.
 *
 * @module
 */

import { type Callbacks, Connection, type ConnectionOptions, type ExecuteResult, finish } from "./client.ts";
import { DEFAULT_HIDDEN, type Volume, checkVolumes } from "./volumes.ts";

/**
 * Limits for a run. Omitted means your org's limit; a value over it is capped
 * at it. eryx.run sets memory and callback limits itself.
 */
export interface ResourceLimits {
  /** How long the run may take, rounded up to whole seconds. */
  timeoutMs?: number;
  /** How much fuel (compute units) the run may use. */
  maxFuel?: number;
}

/** Options for a {@link Sandbox}. */
export interface SandboxOptions extends ConnectionOptions {
  /** Functions the code can `await`, by name. They run in this process. */
  callbacks?: Callbacks;
  /** Limits for each run. */
  limits?: ResourceLimits;
  /** An environment your org uploaded to run in. Default: plain Python. */
  environment?: string;
  /** MCP connections the code can call (`mcp.<connection>.<tool>`). Default: all of your org's. */
  connections?: string[];
  /** Network policies for the code. Default: your org's default ones; `[]` for no network. */
  networkPolicies?: string[];
  /**
   * Local directories the code can read, under `/mnt/`. They're served from
   * this process (Node.js, Bun or Deno) as the code reads them, so nothing is
   * uploaded first.
   */
  volumes?: Volume[];
  /**
   * Paths in `volumes` that the code doesn't see, as patterns. Default
   * {@link DEFAULT_HIDDEN} (`.env`, keys, `.git/config` and the like); `[]`
   * serves everything.
   */
  hidden?: readonly string[];
}

/** Options for one run. */
export interface ExecuteOptions {
  /**
   * Stops waiting for the run when aborted, rejecting with the signal's
   * reason. The run itself carries on on eryx.run until it finishes or hits
   * a limit, and its compute units still count.
   */
  signal?: AbortSignal;
}

/**
 * Runs Python in an isolated sandbox on eryx.run. Each `execute` is a fresh
 * run; use a {@link Session} to keep state between them.
 *
 * @example
 * const sandbox = new Sandbox({
 *   callbacks: { get_user: async ({ id }) => ({ id, name: "Ada" }) },
 * });
 * const result = await sandbox.execute('user = await get_user(id=1)\nprint(user["name"])');
 * console.log(result.stdout); // "Ada\n"
 */
export class Sandbox {
  protected readonly connection: Connection;
  private readonly callbacks: Callbacks;
  private readonly volumes: Volume[];
  private readonly hidden: readonly string[];
  private readonly options: Record<string, unknown>;

  constructor(options: SandboxOptions = {}) {
    this.connection = new Connection(options);
    this.callbacks = options.callbacks ?? {};
    this.volumes = options.volumes ?? [];
    this.hidden = options.hidden ?? DEFAULT_HIDDEN;
    checkVolumes(this.volumes);
    const { timeoutMs, maxFuel } = options.limits ?? {};
    this.options = {
      ...(timeoutMs !== undefined && { timeout_secs: Math.max(1, Math.ceil(timeoutMs / 1000)) }),
      ...(maxFuel !== undefined && { fuel_limit: maxFuel }),
      ...(options.environment !== undefined && { environment: options.environment }),
      ...(options.connections !== undefined && { connections: options.connections }),
      ...(options.networkPolicies !== undefined && { network_policies: options.networkPolicies }),
    };
  }

  /**
   * Run `code` and return its result.
   *
   * @throws {PythonExceptionError} If the code raises an exception.
   * @throws {TimeoutError} If the run exceeds its time limit.
   * @throws {ResourceLimitError} If it uses up its compute units or memory.
   * @throws {APIError} If eryx.run refuses the request.
   */
  async execute(code: string, options: ExecuteOptions = {}): Promise<ExecuteResult> {
    return finish(await this.run(code, undefined, options.signal));
  }

  /** Run `code`, in a session if `sessionId` is given, and return the API's response */
  protected async run(code: string, sessionId: string | undefined, signal?: AbortSignal) {
    const body = { code, ...this.options };
    // Callbacks and volumes are served over the stream
    if (Object.keys(this.callbacks).length || this.volumes.length) {
      return this.connection.stream(
        sessionId === undefined ? body : { ...body, session_id: sessionId },
        this.callbacks,
        this.volumes,
        this.hidden,
        signal,
      );
    }
    const path = sessionId === undefined ? "/v1/execute" : `/v1/sessions/${encodeURIComponent(sessionId)}/execute`;
    return (await this.connection.request("POST", path, body, signal))!;
  }
}

/** Options for a {@link Session}. */
export interface SessionOptions extends SandboxOptions {
  /** How long the session lasts after its last run. Default an hour, at most a day. */
  ttlSecs?: number;
  /** An existing session to use, e.g. one another process created. */
  id?: string;
}

/**
 * Keeps Python state between runs on eryx.run: variables, functions, imports
 * and files under `/data`, like cells in a notebook. The session is created
 * on the first run; {@link Session.delete} ends it. Runs in a session go one
 * at a time: one started while another is running throws a `ConflictError`.
 *
 * @example
 * const session = new Session();
 * await session.execute("x = 21");
 * (await session.execute("print(x * 2)")).stdout; // "42\n"
 * await session.delete();
 */
export class Session extends Sandbox {
  private readonly ttlSecs: number | undefined;
  private readonly environment: string | undefined;
  private created: Promise<string> | undefined;
  private count = 0;

  /** The session's id, once it's created. */
  id: string | undefined;

  constructor(options: SessionOptions = {}) {
    super(options);
    this.ttlSecs = options.ttlSecs;
    this.environment = options.environment;
    this.id = options.id;
    if (options.id !== undefined) this.created = Promise.resolve(options.id);
  }

  /** How many runs this object has made in the session. */
  get executionCount(): number {
    return this.count;
  }

  /** Run `code` in the session. Throws as {@link Sandbox.execute} does; code that raises keeps its changes. */
  override async execute(code: string, options: ExecuteOptions = {}): Promise<ExecuteResult> {
    const response = await this.run(code, await this.ensure(options.signal), options.signal);
    this.count++;
    return finish(response);
  }

  /** End the session now, rather than when it expires. */
  async delete(options: ExecuteOptions = {}): Promise<void> {
    const id = this.created && (await this.created.catch(() => undefined));
    if (id === undefined) return;
    await this.connection.request("DELETE", `/v1/sessions/${encodeURIComponent(id)}`, undefined, options.signal);
    this.created = undefined;
    this.id = undefined;
  }

  /**
   * The session's id, creating it on the first run; runs started meanwhile
   * share the one created, so it doesn't take one caller's signal
   */
  private ensure(signal?: AbortSignal): Promise<string> {
    this.created ??= this.connection
      .request(
        "POST",
        "/v1/sessions",
        {
          ...(this.environment !== undefined && { environment: this.environment }),
          ...(this.ttlSecs !== undefined && { ttl_secs: this.ttlSecs }),
        },
      )
      .then((created) => String(created!.id))
      .catch((e: unknown) => {
        this.created = undefined;
        throw e;
      });
    const created = this.created.then((id) => (this.id = id));
    if (!signal) return created;
    signal.throwIfAborted();
    return Promise.race([
      created,
      new Promise<never>((_, reject) => signal.addEventListener("abort", () => reject(signal.reason), { once: true })),
    ]);
  }
}
