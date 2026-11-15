# pyeryx-runtime-javascript

The precompiled JavaScript guest runtime for [pyeryx](https://pypi.org/project/pyeryx/),
for one platform. pyeryx itself only includes the Python runtime; install this
with `pip install pyeryx[javascript]` and `eryx.Sandbox(language="javascript")` finds it.

Its version must match pyeryx's exactly: a precompiled runtime only loads in
the eryx version that built it.

The wheel is built by copying the target's `runtime-javascript.cwasm` (from
`mise run precompile-runtime-javascript` or the CI artifact) into
`pyeryx_runtime_javascript/` and running `python -m build --wheel`.
