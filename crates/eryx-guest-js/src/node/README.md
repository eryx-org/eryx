# Node built-ins for the JavaScript guest

The `node:*` modules the guest provides, served by its module loader
(`../modules.rs`, `node_module`) and evaluated on first import. They're
compiled into the runtime wasm eryx distributes, so the vendored ones carry
their packages' full licences in a comment at the top of the module.

| Module | Source | Licence |
|--------|--------|---------|
| `node:buffer` | [buffer] 6.0.3, with [base64-js] 1.5.1 and [ieee754] 1.2.1 | MIT, BSD-3-Clause (ieee754) |
| `node:events` | [events] 3.3.0 | MIT |
| `node:path` (and `path/posix`) | [path-browserify] 1.0.1 | MIT |
| `node:string_decoder` | [string_decoder] 1.3.0, with [safe-buffer] 5.2.1 | MIT |
| `node:util` | `util.js`, written for eryx: a subset | (eryx's) |
| `node:crypto` | `crypto.js`, written for eryx: random values only | (eryx's) |

- `licenses/`: each vendored package's licence file, from its npm tarball.
- `regen.sh` (with `regen.mjs`) installs the pinned versions above and
  rebuilds the vendored modules with esbuild (0.28.2 when last run): each
  package bundled into one minified ES module, with named exports so static
  imports link, and its licences in front. Needs the npm registry and node.

The other built-ins (`fs`, `net`, `child_process`, `http`, `url`, ...) throw
"node:… isn't available in eryx yet" on import.

[buffer]: https://github.com/feross/buffer
[base64-js]: https://github.com/beatgammit/base64-js
[ieee754]: https://github.com/feross/ieee754
[events]: https://github.com/browserify/events
[path-browserify]: https://github.com/browserify/path-browserify
[string_decoder]: https://github.com/nodejs/string_decoder
[safe-buffer]: https://github.com/feross/safe-buffer
