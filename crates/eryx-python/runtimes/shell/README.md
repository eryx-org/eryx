# pyeryx-runtime-shell

The precompiled shell guest runtime for [pyeryx](https://pypi.org/project/pyeryx/),
for one platform, with the shell's default external commands (uutils
coreutils: cat, head, ls, sort, ...). pyeryx itself only includes the Python
runtime; install this with `pip install pyeryx[shell]` and
`eryx.Sandbox(language="shell")` finds both.

Its version must match pyeryx's exactly: a precompiled runtime only loads in
the eryx version that built it.

The wheel is built by copying the target's `runtime-shell.cwasm` and
`coreutils.cwasm` (from `mise run precompile-runtime-shell` and
`mise run precompile-coreutils`, or the CI artifacts) into
`pyeryx_runtime_shell/` and running `python -m build --wheel`.
