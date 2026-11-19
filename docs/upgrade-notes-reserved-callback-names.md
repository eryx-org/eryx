# Upgrade notes: callback names a guest already uses (#962)

These notes are for the release that ships #962 (#970 and #991). Copy them
into the release PR's CHANGELOG entry by hand: release-plz doesn't generate
them, and they have to be re-applied if release-plz regenerates the PR.

## A callback can't be named like something the guest has

A guest installs each callback under its name (for a dotted name like
`http.get`, under its first part, `http`), and keeps what it already has. In
Python, a callback named `print` or `open` could therefore only be reached
through `invoke("print")`. Building a sandbox with such a callback now fails,
in every guest, naming the callback and what it collides with:

- **Python:** the builtins (`print`, `list`, `open`, `sum`, ...) and the
  names the guest defines (`invoke`, `list_callbacks`, `asyncio`, `json`,
  `os`, ...), also as a dotted name's first part (`json.dumps`).
- **JavaScript:** the guest's globals (`fetch`, `crypto`, `URL`,
  `setTimeout`, `console`, `require`, `invoke`, `process`, ...), also as a
  dotted name's first part.
- **Shell:** the builtins (`echo`, `cd`, `test`, ..., and `invoke`, `grep`),
  the reserved words (`time`, `function`, `[[`, ...), and the sandbox's
  commands (the embedded coreutils, `with_command` ones).

Where it fails:

- Rust: `SandboxBuilder::build()` and `Sandbox::set_callbacks` return
  `ErrorKind::Initialization`. That includes a pool's and the gRPC server's
  per-request callbacks, which fail the request.
- pyeryx: building a `Sandbox`, `Session`, factory sandbox or pooled sandbox
  raises `InitializationError`.
- `@eryx-org/eryx`: `new Sandbox()` throws a `TypeError`.

**Breaking for Python users:** released pyeryx and Rust Python sandboxes
accepted these names, and the callback was reachable through `invoke()`.
That now fails at build time instead.

**Migration:** rename the callback (`fetch_user`, `print_report`), or put it
under a prefix (`tools.print`). Names that only resemble a reserved one
(`print_report`, `mcp.fetch.*`) still build. The lists are in
`crates/eryx/src/reserved/`, and the book's callbacks page has a Reserved
Names section.
