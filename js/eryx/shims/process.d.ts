/** The tools the bundled coreutils provides to a shell sandbox. */
export const COREUTILS_TOOLS: readonly string[];

/** A jco-transpiled command: its `instantiate`, and where its core modules come from. */
export interface CommandModule {
  instantiate: (getCoreModule: (name: string) => unknown, imports: object) => Promise<unknown>;
  getCoreModule: (name: string) => WebAssembly.Module | Promise<WebAssembly.Module>;
}

/** The shell guest's `eryx:process` imports for one sandbox. */
export function createProcessHost(
  fileTree: object,
  commands: Map<string, () => Promise<CommandModule>>,
): {
  Child: {
    spawn(cmd: string, args: string[], env: [string, string][], cwd: string, stdin: unknown): {
      child: unknown;
      stdout: ReadableStream<Uint8Array>;
      stderr: ReadableStream<Uint8Array>;
    };
  };
  listCommands(): string[];
  /** Start an execution's fresh limits on live children and buffered bytes. */
  startExecution(): void;
};
