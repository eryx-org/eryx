import type { Sandbox, ExecuteResult } from "@bsull/eryx";

export type SandboxState =
  | { status: "loading" }
  | { status: "relinking"; progress: string }
  | { status: "error"; message: string }
  | { status: "ready"; sandbox: Sandbox; loadTime: string };

export type { ExecuteResult };

let sandboxState: SandboxState = $state({ status: "loading" });

export function getSandboxState(): SandboxState {
  return sandboxState;
}

export function setSandboxState(state: SandboxState): void {
  sandboxState = state;
}

export async function initSandbox(): Promise<void> {
  if (typeof (WebAssembly as any)?.Suspending !== "function") {
    sandboxState = {
      status: "error",
      message:
        "Your browser does not support WebAssembly JSPI. Please use Chrome 133+ or Edge 133+.",
    };
    return;
  }

  const start = performance.now();
  try {
    const { Sandbox } = await import("@bsull/eryx");
    const sandbox = new Sandbox();
    const loadTime = ((performance.now() - start) / 1000).toFixed(1);
    sandboxState = { status: "ready", sandbox, loadTime };
  } catch (e) {
    sandboxState = {
      status: "error",
      message: `Failed to load sandbox: ${(e as Error).message}`,
    };
  }
}

/**
 * Replace the current sandbox with a dynamically linked one.
 *
 * The factory function receives a progress callback and should return an
 * object with `execute`, `snapshotState`, `restoreState`, `clearState`,
 * and `finalizePreinit` methods (same shape as the jco-generated exports).
 */
export async function replaceSandbox(
  factory: (onProgress: (msg: string) => void) => Promise<DynamicSandboxExports>,
): Promise<void> {
  sandboxState = { status: "relinking", progress: "Starting..." };

  const start = performance.now();
  try {
    const exports = await factory((msg) => {
      sandboxState = { status: "relinking", progress: msg };
    });

    const loadTime = ((performance.now() - start) / 1000).toFixed(1);
    sandboxState = {
      status: "ready",
      sandbox: new DynamicSandbox(exports),
      loadTime,
    };
  } catch (e) {
    sandboxState = {
      status: "error",
      message: `Failed to link extensions: ${(e as Error).message}`,
    };
  }
}

export async function runCode(
  code: string,
): Promise<
  | { ok: true; result: ExecuteResult; elapsed: number }
  | { ok: false; error: string; elapsed: number }
> {
  if (sandboxState.status !== "ready") {
    return { ok: false, error: "Sandbox not ready", elapsed: 0 };
  }
  const start = performance.now();
  try {
    const result = await sandboxState.sandbox.execute(code);
    return { ok: true, result, elapsed: performance.now() - start };
  } catch (e) {
    return {
      ok: false,
      error: (e as Error).message,
      elapsed: performance.now() - start,
    };
  }
}

/** Shape of exports from a dynamically linked + transpiled sandbox. */
export interface DynamicSandboxExports {
  execute: (code: string) => Promise<any>;
  snapshotState: () => Promise<Uint8Array>;
  restoreState: (data: Uint8Array) => Promise<void>;
  clearState: () => Promise<void>;
  finalizePreinit: () => void;
}

/** Wraps dynamically linked exports to match the Sandbox interface. */
class DynamicSandbox implements Sandbox {
  #exports: DynamicSandboxExports;

  constructor(exports: DynamicSandboxExports) {
    this.#exports = exports;
  }

  async execute(code: string): Promise<ExecuteResult> {
    const output = await this.#exports.execute(code);
    if (output.tag === "error") {
      throw new Error(output.val);
    }
    return { stdout: output.stdout, stderr: output.stderr };
  }

  async snapshotState(): Promise<Uint8Array> {
    return this.#exports.snapshotState();
  }

  async restoreState(data: Uint8Array): Promise<void> {
    await this.#exports.restoreState(data);
  }

  async clearState(): Promise<void> {
    await this.#exports.clearState();
  }
}
