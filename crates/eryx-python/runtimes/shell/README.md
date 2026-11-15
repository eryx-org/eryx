# pyeryx-runtime-shell

The precompiled shell guest runtime for [pyeryx](https://pypi.org/project/pyeryx/),
for one platform. pyeryx itself only includes the Python runtime; install this
with `pip install pyeryx[shell]` and `eryx.Sandbox(language="shell")` finds it.

Its version must match pyeryx's exactly: a precompiled runtime only loads in
the eryx version that built it.

The wheel is built by copying the target's `runtime-shell.cwasm` (from
`mise run precompile-runtime-shell` or the CI artifact) into
`pyeryx_runtime_shell/` and running `python -m build --wheel`.
