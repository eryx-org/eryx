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

/** Most bytes of stdin a child is given; past that its spawn fails with `limit-exceeded`. */
const MAX_STDIN_BYTES = 16 * 1024 * 1024;

/** Most children of a sandbox spawned and not yet finished, as the Rust host's. */
const MAX_LIVE_CHILDREN = 64;

/**
 * Most bytes a sandbox's children hold at once in this host: their stdin
 * buffers, and output the shell hasn't read yet.
 */
const MAX_BUFFERED_BYTES = 64 * 1024 * 1024;

/** Bytes asked for per read of a child's stdin. */
const STDIN_READ_BYTES = 64 * 1024;

/** A failure to report to the guest as `process-error` `payload`. */
const fail = (payload) => ({ payload });

/** A count with a limit: what a sandbox's children hold. */
class Budget {
  used = 0;
  constructor(limit) {
    this.limit = limit;
  }
  /** Take `n`, if that stays within the limit. */
  take(n) {
    if (this.used + n > this.limit) return false;
    this.used += n;
    return true;
  }
  give(n) {
    this.used -= n;
  }
}

/**
 * Read a stream the guest passed in to its end, charging `bytes` for what it
 * holds.
 *
 * With `read(count)`, not `for await`: jco's async iterator over a
 * `stream<u8>` yields one byte per step, which runs at about 0.1 MiB/s and
 * exhausts the heap on a few MiB.
 * @param {{ read(count: number): Promise<{ value?: Uint8Array, done: boolean }> }} stream
 * @param {Budget} bytes
 * @returns {Promise<Uint8Array>} Charged to `bytes` (give back its length when done)
 */
async function readAll(stream, bytes) {
  const chunks = [];
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await stream.read(STDIN_READ_BYTES);
      if (done) break;
      if (size + value.byteLength > MAX_STDIN_BYTES || !bytes.take(value.byteLength)) {
        throw fail("limit-exceeded");
      }
      chunks.push(value);
      size += value.byteLength;
    }
  } catch (e) {
    bytes.give(size);
    // The shell drops its end once the child's `wait` returns; drop ours too,
    // or jco keeps the stream until the instance goes.
    try {
      stream.return?.()?.catch?.(() => {});
    } catch {
      // Already gone.
    }
    throw e?.payload ? e : fail("io-error");
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
 * A child's stdout or stderr: a stream the shell reads, fed by the child's
 * writes. Each write is charged to `bytes` until the shell reads it, and a
 * write past {@link MAX_OUTPUT_BYTES} in all, or past the budget, stops the
 * child, which the shell reports as crashed (`<name>: spawn-failed`, 134).
 * @param {Budget} bytes
 */
function outputPipe(bytes) {
  const queue = [];
  let written = 0;
  let closed = false;
  let wake = () => {};
  const stream = new ReadableStream(
    {
      async pull(controller) {
        while (queue.length === 0 && !closed) await new Promise((resolve) => (wake = resolve));
        if (queue.length === 0) return controller.close();
        const chunk = queue.shift();
        bytes.give(chunk.byteLength);
        controller.enqueue(chunk);
      },
      cancel() {
        for (const chunk of queue.splice(0)) bytes.give(chunk.byteLength);
      },
    },
    // Pulled only when the shell reads, so unread output stays in `queue`.
    { highWaterMark: 0 },
  );
  const handler = {
    write(buf) {
      written += buf.byteLength;
      if (written > MAX_OUTPUT_BYTES || !bytes.take(buf.byteLength)) {
        throw new Error("a command's output is over its limit");
      }
      // Copied: the buffer is a view of the child's memory.
      queue.push(buf.slice());
      wake();
    },
  };
  const close = () => {
    closed = true;
    wake();
  };
  return { stream, handler, close };
}

/**
 * @typedef {Object} CommandModule
 * @property {Function} instantiate - The jco-generated `instantiate(getCoreModule, imports)`
 * @property {(name: string) => WebAssembly.Module | Promise<WebAssembly.Module>} getCoreModule
 */

/**
 * The `eryx:process` imports for one sandbox, and `startExecution()`, to
 * call as each execution starts.
 *
 * The limits on live children and buffered bytes are per execution, as the
 * Rust host's: each execution starts with fresh budgets, and a child an
 * earlier one left behind (it was cancelled mid-run) charges and frees the
 * budgets of the execution that spawned it.
 * @param {Object} fileTree - The sandbox's file tree, which children share
 * @param {Map<string, () => Promise<CommandModule>>} commands - Loaders by command name
 * @returns {{ Child: Function, listCommands: () => string[], startExecution: () => void }}
 */
export function createProcessHost(fileTree, commands) {
  let live;
  let bytes;
  const startExecution = () => {
    live = new Budget(MAX_LIVE_CHILDREN);
    bytes = new Budget(MAX_BUFFERED_BYTES);
  };
  startExecution();

  class Child {
    /** @type {(() => Promise<number>) | null} */
    #run;
    /** The live-children budget the child holds a slot of. */
    #slots;
    #released = false;

    constructor(run, slots) {
      this.#run = run;
      this.#slots = slots;
    }

    static spawn(cmd, args, env, cwd, stdin) {
      const load = commands.get(cmd);
      if (!load) throw fail("command-not-found");
      // This execution's budgets, which the child keeps however long it runs.
      const slots = live;
      const held = bytes;
      if (!slots.take(1)) throw fail("limit-exceeded");
      const stdout = outputPipe(held);
      const stderr = outputPipe(held);
      let child;
      const run = async () => {
        let input;
        try {
          let command;
          try {
            command = await load();
          } catch {
            // A bad URL, or a download that failed: as a command that can't start.
            throw fail("spawn-failed");
          }
          input = await readAll(stdin, held);
          return await runChild(command, fileTree, {
            argv: [cmd, ...args],
            env,
            cwd,
            stdin: input,
            stdout: stdout.handler,
            stderr: stderr.handler,
          });
        } finally {
          if (input) held.give(input.byteLength);
          stdout.close();
          stderr.close();
          child.release();
        }
      };
      child = new Child(run, slots);
      return { child, stdout: stdout.stream, stderr: stderr.stream };
    }

    /** Give back the child's live slot, once. */
    release() {
      if (!this.#released) this.#slots.give(1);
      this.#released = true;
    }

    async wait() {
      const run = this.#run;
      if (!run) throw fail("io-error");
      this.#run = null;
      return run();
    }

    // The shell dropped the child: one never waited for frees its slot.
    [Symbol.dispose ?? Symbol.for("dispose")]() {
      if (this.#run) this.release();
    }
  }

  // The shell's own `grep` builtin and nested `sh`/`bash` run only when
  // they're listed; with no allow-list here, they always are.
  const listCommands = () => [...commands.keys(), "grep", "sh", "bash"];
  return { Child, listCommands, startExecution };
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
    throw fail("spawn-failed");
  }
  try {
    await instance.run.run();
  } catch (e) {
    // `exit(n)` throws to unwind the child.
    if (e?.exitError) return e.code;
    // `run` returning its error (a C `main` returning nonzero, for one) is
    // jco's ComponentError, with the (empty) error as `payload`: exit status
    // 1, as the Rust host gives it. Anything else is a crash.
    if (e instanceof Error && Object.hasOwn(e, "payload")) return 1;
    throw fail("spawn-failed");
  }
  return 0;
}
