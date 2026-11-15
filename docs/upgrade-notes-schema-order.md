# Upgrade notes: positional arguments follow declaration order (#860)

These notes are for the release that ships #860. Copy them into the release
PR's CHANGELOG entry by hand: release-plz doesn't generate them, and they have
to be re-applied if release-plz regenerates the PR.

## Positional callback arguments bind in declaration order

Sandboxed code can pass a callback's arguments by position (`await f(1, 2)`).
They bind to the callback's parameters in the order of its schema's
`properties`. Until now that order was alphabetical, whatever order the
schema declared them in: eryx serialized schemas through `serde_json`, which
sorted object keys. They now keep their declaration order.

For a callback whose parameters are declared `zip, city`, `await f("10001",
"NYC")` used to bind `city="10001", zip="NYC"`; it now binds `zip="10001",
city="NYC"`. This applies to every guest (Python, JavaScript and the shell) and
every way of declaring a schema: `TypedCallback` structs (field order),
`DynamicCallback` builders (`.param()` order), JSON schemas from pyeryx and the
gRPC server (key order as given), and Python signatures in pyeryx.

Calls that only use keyword arguments, and callbacks whose parameters are
declared in alphabetical order, are unaffected. Check callers that pass
arguments by position to callbacks with other parameter orders.

## JSON key order elsewhere

The fix enables serde_json's `preserve_order` feature, so JSON objects that
eryx builds keep their keys in insertion order instead of sorting them:

- Callback schemas, wherever they are shown: `list_callbacks()` in the guest
  and the parameters schema JSON the host sends guests.
- Callback results that a host builds as objects, and the JSON eryx
  re-serializes (callback journal results, the gRPC `ExecuteResult` journal),
  now keep the order the value was built in. A Python callback returning
  `{"b": 1, "a": 2}` reaches the sandboxed code with `b` first.

Code that compares this JSON as text, rather than parsing it, may need
updating. The values themselves are unchanged.

**Rust users:** `eryx` now depends on serde_json with `preserve_order`. Cargo
unifies features, so every crate in a build that uses eryx gets it too:
`serde_json::Map` becomes insertion-ordered there, and `Value::Object`
serializes in insertion order. Code that relied on serde_json sorting object
keys should sort them itself.

## Unchanged

- **Callback replay journals.** Entries are keyed on the guest's argument
  text, which the Python guest sorts itself. Where the host makes the key
  (positional arguments it binds, a direct `ReplayCallback::invoke` call) it
  still sorts the keys, so journals recorded before this release replay as
  before.
- **Signed journals** from eryx-server verify over the bytes they were signed
  with, so ones signed before the upgrade still verify.
- **Snapshots, precompiled runtimes and their cache keys** don't depend on
  JSON key order. (A precompiled runtime or saved factory only loads in the
  eryx version that built it anyway.)
