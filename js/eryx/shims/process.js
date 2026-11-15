/**
 * `eryx:process` for the shell guest, with no commands: this package can't run
 * child components (yet), so `list-commands` is empty and every spawn is
 * `command-not-found`. The shell reports that as exit 127, as the Rust host does
 * when no commands are registered.
 *
 * jco lowers `result<T, E>` imports as: return T, or throw `{ payload: E }`.
 */

/** A child process; never constructed, since spawning always fails. */
export class Child {
  static spawn() {
    throw { payload: "command-not-found" };
  }
  writeStdin() {
    throw { payload: "io-error" };
  }
  closeStdin() {}
  readStdout() {
    throw { payload: "io-error" };
  }
  readStderr() {
    throw { payload: "io-error" };
  }
  wait() {
    throw { payload: "io-error" };
  }
}

/** The commands the shell can spawn: none. */
export function listCommands() {
  return [];
}
