# Bundled npm packages for `tests/js_environment.rs`

Minified ES-module bundles of three npm packages, as eryx's bundler
(`crates/eryx-bundler`, rolldown 1.3.2) produces them, so the JavaScript
environment tests can import real packages without network access. The test
builds an environment around `modules/` (writing its manifest) and imports
them from a sandbox.

| Package  | Version | Licence | Licence text |
|----------|---------|---------|--------------|
| zod      | 4.6.7   | MIT     | `licenses/zod-LICENSE` |
| date-fns | 4.4.0   | MIT     | `licenses/date-fns-LICENSE.md` |
| lodash   | 4.18.1  | MIT     | `licenses/lodash-LICENSE` |

- `modules/`: one module per package, plus the bundler's shared runtime chunk.
- `bundle.json`: the bundler's report, mapping each package to its module.
- `licenses/`: each package's licence file, copied from its npm tarball.

Regenerate everything with `./regen.sh` (needs the npm registry and
wasmtime). These are test data, not vendored dependencies: eryx doesn't ship
them.
