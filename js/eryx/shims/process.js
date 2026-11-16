/**
 * `eryx:process` for the shell guest, with no commands: this package can't run
 * child components (yet), so `list-commands` names only the shell's own `grep`
 * builtin and every spawn is `command-not-found`. The shell reports that as exit 127, as the Rust host does
 * when no commands are registered.
 *
 * jco lowers `result<T, E>` imports as: return T, or throw `{ payload: E }`.
 */

/** A child process; never constructed, since spawning always fails. */
export class Child {
  static spawn() {
    throw { payload: "command-not-found" };
  }
  wait() {
    throw { payload: "io-error" };
  }
}

/**
 * The commands the shell can run: no child components, but the shell's own
 * `grep` builtin, which it runs only when the host lists it (the Rust host
 * leaves it out of a narrowed `allowed_commands`).
 */
export function listCommands() {
  return ["grep"];
}
