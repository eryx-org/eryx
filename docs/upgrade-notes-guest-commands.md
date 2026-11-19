# Upgrade notes: guests as commands (#831)

These notes are for the release that ships #831's `run-command` export and
the guest commands built on it. Copy them into the release PR's CHANGELOG
entry by hand: release-plz doesn't generate them, and they have to be
re-applied if release-plz regenerates the PR.

## Custom runtimes: the `sandbox` world exports `run-command`

The `eryx:sandbox/sandbox` world has a new export:

```wit
export run-command: async func(args: list<string>, env: list<tuple<string, string>>, cwd: string) -> s32;
```

It runs the guest as a command-line program (`python3 -c ...`, `sh
script.sh`) on a fresh instance: `args` is the full argv, `env` and `cwd`
replace the guest's environment and working directory, the program reads and
writes WASI stdin, stdout and stderr directly, and the result is its exit
status. A guest that can't run as a command writes why to stderr and returns
127.

**A runtime built against the old world fails to load** (`SandboxPre`
type-checking fails on the missing export). The runtimes eryx builds, embeds
and publishes all have it, and precompiled runtimes from an earlier release
are already refused by the version stamp, so this only affects a guest you
build yourself from `runtime.wit`: add the export (returning 127 is enough)
and rebuild it.

The Python runtime implements it as `python3`: `-c CODE`, `-m MODULE`, a
script path, `-` or no argument (the program comes from stdin), `-V`, and the
no-op flags `-u`, `-B`, `-I`, `-E`, `-s` and `-q`; anything else exits 2. A
script's top-level `asyncio.run(main())` runs on the guest's event loop, as in
`execute`. The shell runtime implements it as `sh`/`bash` (`-c`, a script,
stdin, waiting for its background jobs). The JavaScript runtime returns 127
for now.

## Python as a shell command

New, and opt-in: nothing registers it by default.

- **Rust:** `Command::guest(Language::Python)` (with `embedded` and
  `shell` or `unstable-vfs`) and `unsafe Command::guest_from_precompiled_file(language,
  path, stdlib)` run a Python runtime as a command. Register it with
  `with_commands(["python", "python3"], ...)`.
- **pyeryx:** `eryx.Command.python()`, passed as
  `commands={"python": py, "python3": py}`.
- **eryx-server:** `--shell-python` (`ERYX_SHELL_PYTHON`) registers `python`
  and `python3` from the server's Python runtime, and requests'
  `allowed_commands` and `commands` can name them.

Each run is a fresh interpreter on the shell's files, env, cwd and stdio,
within the execution's fuel, timeout and the children's 256 MiB memory pool.
It has no callbacks and no network, and a credential can't be for it: the
sandbox fails to build or the execution to start, naming the command.

## The shell as a command, on by default

A command registered with `allow_spawn` (git) that runs `sh -c ...` or
`bash` used to get `command-not-found`. The embedded shell now registers
itself as `sh` and `bash` for such commands (`Command::guest(Language::Shell)`):
a fresh shell guest per run, running only what the command itself may run,
one nesting level deeper, within its network, with no callbacks or network
of its own. The shell's own `sh`/`bash` still run in its guest.

- `without_embedded_commands()` (and eryx-server's
  `--no-shell-embedded-commands`) leaves it out with coreutils.
- An execution's allowed commands govern both: allow `sh`/`bash` for a
  child to run them. An allow-list that names `git` but not `sh` makes git's
  `sh -c` fail as before.
- A child's `sh` counts towards the nesting depth of 4, so a chain that
  just fit before can be one level too deep if it goes through `sh`.

Guest commands (`python3` and `sh`) now also get the execution's deadline,
so `eryx:process/execution`'s `time-left` returns the real time left in
them instead of none.

Jobs a nested in-guest shell leaves running (`sh -c 'slow &'`) are now
waited for at the end of the execution, and killed at the timeout with the
usual stderr line, like the script's own jobs. Before, the execution's end
didn't wait for them.

## eryx-server: `--no-shell-coreutils` is now `--no-shell-embedded-commands`

The flag (and its variable, `ERYX_NO_SHELL_COREUTILS`, now
`ERYX_NO_SHELL_EMBEDDED_COMMANDS`) is renamed, because it covers all of the
shell's embedded commands, not just coreutils. **The old names are gone**: a
server started with the old flag fails to parse its arguments, and one with
the old variable ignores it and keeps coreutils. Rename both in deployments.
