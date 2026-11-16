/**
 * `eryx:process` for the shell guest: external commands as child instances of
 * jco-transpiled WASI 0.2 command components (`wasi:cli/run`), such as the
 * bundled uutils coreutils.
 *
 * A child runs in batch, as one call: `spawn` only records it, and `wait`
 * reads its stdin to the end, runs a fresh instance to completion, and then
 * hands its stdout and stderr to the shell. So pipeline stages run one after
 * another rather than together (the shell waits on all of them at once, so a
 * pipeline still completes), output arrives when a stage ends, and an endless
 * producer (`yes | head`) runs until the sandbox's `timeoutMs`. A child shares
 * the sandbox's in-memory filesystem, and has no network.
 *
 * jco lowers `result<T, E>` imports as: return T, or throw `{ payload: E }`.
 */

import { createCli } from "#preview2/cli";
import { monotonicClock, wallClock } from "#preview2/clocks";
import { createFilesystem, InMemoryFilesystemAdapter } from "#preview2/filesystem";
import { error, poll, pollableCreate, streams } from "#preview2/io";
import { insecureSeed, random } from "#preview2/random";

/**
 * The tools the bundled coreutils provides, as the Rust crate's
 * `eryx::embedded::COREUTILS_TOOLS` (keep the two in step).
 */
export const COREUTILS_TOOLS = Object.freeze([
  "cat", "head", "tail", "ls", "wc", "cp", "mv", "rm", "mkdir", "touch", "sort", "seq", "cut",
  "tr", "uniq", "basename", "dirname", "tee", "sleep", "paste", "comm", "date", "base64",
  "realpath", "readlink", "rmdir",
]);

/** Most bytes a child may write to stdout, and to stderr, as the Rust host's per-stream cap. */
const MAX_OUTPUT_BYTES = 16 * 1024 * 1024;

/** Bytes asked for per read of a child's stdin. */
const STDIN_READ_BYTES = 64 * 1024;

/**
 * Read a stream the guest passed in to its end.
 *
 * With `read(count)`, not `for await`: jco's async iterator over a
 * `stream<u8>` yields one byte per step, which runs at about 0.1 MiB/s and
 * exhausts the heap on a few MiB.
 * @param {{ read(count: number): Promise<{ value?: Uint8Array, done: boolean }> }} stream
 * @returns {Promise<Uint8Array>}
 */
async function readAll(stream) {
  const chunks = [];
  let size = 0;
  for (;;) {
    const { value, done } = await stream.read(STDIN_READ_BYTES);
    if (done) break;
    chunks.push(value);
    size += value.byteLength;
  }
  const out = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

/**
 * A child's stdin: `bytes`, then end-of-stream. As preview2-shim's
 * `bufferedInputStream`, which bundlers can't all import from its helpers path.
 * @param {Uint8Array} bytes
 */
function bufferedStdin(bytes) {
  let offset = 0;
  return {
    blockingRead(len) {
      if (offset >= bytes.byteLength) throw { tag: "closed" };
      const end = Math.min(bytes.byteLength, offset + Number(len));
      const chunk = bytes.slice(offset, end);
      offset = end;
      return chunk;
    },
    subscribe: () => pollableCreate(),
  };
}

/**
 * An output stream for a child: what it writes goes to `controller`, up to
 * {@link MAX_OUTPUT_BYTES}. The write past that stops the child, which the
 * shell reports as crashed (`<name>: spawn-failed`, status 134).
 * @param {ReadableStreamDefaultController<Uint8Array>} controller
 */
function cappedOutput(controller) {
  let written = 0;
  return {
    write(buf) {
      written += buf.byteLength;
      if (written > MAX_OUTPUT_BYTES) {
        throw new Error(`a command's output is limited to ${MAX_OUTPUT_BYTES} bytes`);
      }
      // Copied: the buffer is a view of the child's memory.
      controller.enqueue(buf.slice());
    },
  };
}

/**
 * @typedef {Object} CommandModule
 * @property {Function} instantiate - The jco-generated `instantiate(getCoreModule, imports)`
 * @property {(name: string) => WebAssembly.Module | Promise<WebAssembly.Module>} getCoreModule
 */

/**
 * The `eryx:process` imports for one sandbox.
 * @param {Object} fileTree - The sandbox's file tree, which children share
 * @param {Map<string, () => Promise<CommandModule>>} commands - Loaders by command name
 * @returns {{ Child: Function, listCommands: () => string[] }}
 */
export function createProcessHost(fileTree, commands) {
  class Child {
    /** @type {(() => Promise<number>) | null} */
    #run;

    constructor(run) {
      this.#run = run;
    }

    static spawn(cmd, args, env, cwd, stdin) {
      const load = commands.get(cmd);
      if (!load) throw { payload: "command-not-found" };
      let stdoutController;
      let stderrController;
      const stdout = new ReadableStream({ start: (c) => (stdoutController = c) });
      const stderr = new ReadableStream({ start: (c) => (stderrController = c) });
      const run = async () => {
        try {
          return await runChild(await load(), fileTree, {
            argv: [cmd, ...args],
            env,
            cwd,
            stdin: await readAll(stdin),
            stdout: cappedOutput(stdoutController),
            stderr: cappedOutput(stderrController),
          });
        } finally {
          stdoutController.close();
          stderrController.close();
        }
      };
      return { child: new Child(run), stdout, stderr };
    }

    async wait() {
      const run = this.#run;
      if (!run) throw { payload: "io-error" };
      this.#run = null;
      return run();
    }
  }

  // The shell's own `grep` builtin runs only when it's listed; with no
  // allow-list here, it always is.
  const listCommands = () => [...commands.keys(), "grep"];
  return { Child, listCommands };
}

/**
 * Run one child to completion and return its exit code.
 * @param {CommandModule} command
 * @param {Object} fileTree
 * @param {{ argv: string[], env: [string, string][], cwd: string, stdin: Uint8Array,
 *   stdout: Object, stderr: Object }} io
 */
async function runChild(command, fileTree, io) {
  const cli = createCli({
    arguments: io.argv,
    environment: Object.fromEntries(io.env),
    initialCwd: io.cwd,
    stdin: bufferedStdin(io.stdin),
    stdout: io.stdout,
    stderr: io.stderr,
  });
  const fs = createFilesystem({
    adapter: new InMemoryFilesystemAdapter(),
    preopens: { "/": fileTree },
  });
  let instance;
  try {
    instance = await command.instantiate(command.getCoreModule, {
      "wasi:cli/environment": cli.environment,
      "wasi:cli/exit": cli.exit,
      "wasi:cli/stdin": cli.stdin,
      "wasi:cli/stdout": cli.stdout,
      "wasi:cli/stderr": cli.stderr,
      "wasi:cli/terminal-input": cli.terminalInput,
      "wasi:cli/terminal-output": cli.terminalOutput,
      "wasi:cli/terminal-stderr": cli.terminalStderr,
      "wasi:cli/terminal-stdin": cli.terminalStdin,
      "wasi:cli/terminal-stdout": cli.terminalStdout,
      "wasi:clocks/monotonic-clock": monotonicClock,
      "wasi:clocks/wall-clock": wallClock,
      "wasi:filesystem/preopens": fs.preopens,
      "wasi:filesystem/types": fs.types,
      "wasi:io/error": error,
      "wasi:io/poll": poll,
      "wasi:io/streams": streams,
      "wasi:random/insecure-seed": insecureSeed,
      "wasi:random/random": random,
    });
  } catch {
    // Not a WASI 0.2 command, or it needs imports children don't get
    // (network): the shell reports it as a crashed command.
    throw { payload: "spawn-failed" };
  }
  try {
    instance.run.run();
  } catch (e) {
    // `exit(n)` throws to unwind the child; anything else is a crash.
    if (e?.exitError) return e.code;
    throw { payload: "spawn-failed" };
  }
  return 0;
}
