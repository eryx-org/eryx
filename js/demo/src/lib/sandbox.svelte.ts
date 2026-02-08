import type { Sandbox, ExecuteResult } from "@bsull/eryx";

export type SandboxState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; sandbox: Sandbox; loadTime: string };

export type { ExecuteResult };

let sandboxState: SandboxState = $state({ status: "loading" });

export function getSandboxState(): SandboxState {
  return sandboxState;
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
