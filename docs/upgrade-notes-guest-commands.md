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
