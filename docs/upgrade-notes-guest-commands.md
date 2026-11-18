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
`execute`. The JavaScript and shell runtimes return 127 for now.

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

## eryx-server: `--no-shell-coreutils` is now `--no-shell-embedded-commands`

The flag (and its variable, `ERYX_NO_SHELL_COREUTILS`, now
`ERYX_NO_SHELL_EMBEDDED_COMMANDS`) is renamed, because it covers all of the
shell's embedded commands, not just coreutils. **The old names are gone**: a
server started with the old flag fails to parse its arguments, and one with
the old variable ignores it and keeps coreutils. Rename both in deployments.
