# Environments

An environment is a set of packages built ahead of time, in CI or a deploy
step, into a directory that sandboxes mount read-only. Sandboxes never see
package names or a registry: they import what the environment provides.
JavaScript is supported today (npm packages); Python and the shell will use
the same definitions and the same `with_environment`.

## Defining an environment

An environment is described by a TOML file:

```toml
# data-tools.toml
name = "data-tools"
language = "javascript"

[npm]
packages = ["zod@^4", "date-fns", "lodash"]
entries = ["date-fns/format"]    # importable beyond each package's root
# registry = "https://mirror.example/npm/"
# min-release-age = "3d"          # skip versions published more recently (default 3d)
# lockfile = "data-tools.lock"    # default: <name>.lock next to the definition

[limits]                         # optional
max-packages = 500
max-package-bytes = "32MiB"
max-total-bytes = "1GiB"
```

## Building it

```bash
eryx-precompile env build data-tools.toml --out envs/data-tools
```

This resolves the packages, writing `data-tools.lock` (or installing exactly
what an existing one pins), downloads and verifies them, and bundles them into
ES modules with rolldown, which runs as a sandboxed WebAssembly command. Pass
`--frozen` in CI to fail instead of re-resolving when the lockfile is out of
date, and `--offline` to use only the lockfile and the download cache.
Nothing from a package runs during the build: there are no install scripts.

In Rust, the same is `eryx::environment::build` with the `packages` feature.

> Released `eryx-precompile` binaries don't include `env build` yet. From a
> checkout of eryx: `mise run precompile-bundler`, then
> `cargo install --path crates/eryx-precompile --features env`.

The result is a directory with a manifest (`eryx-environment.json`: language,
inputs, what it exports, and a hash of everything sandboxes rely on), the
lockfile and the modules. Rebuilding from the same lockfile gives the same
hash.

## Using it

```rust,ignore
use eryx::environment::Environment;
use eryx::Sandbox;

// Pin the hash CI built, so a different environment at the path is refused.
let environment = Environment::load_expecting("envs/data-tools", "sha256-…")?;
let sandbox = Sandbox::embedded()
    .javascript()
    .with_environment(&environment)
    .build()?;

sandbox.execute(r#"
    import { z } from "zod";
    import { format } from "date-fns";
    import _ from "lodash";
    console.log(format(new Date(0), "yyyy-MM-dd"), _.chunk([1, 2, 3], 2));
"#).await?;
```

Loading and mounting only need eryx's `environment` feature, which includes
neither the bundler nor an HTTP client. `build()` refuses an environment for
another language, and checks the environment still hashes to what it did
when it was loaded, as pools and factories do for every sandbox they make.

## Importing in JavaScript

- **`import` declarations** at the top of a script work as usual: eryx
  rewrites them into dynamic imports, since scripts run as global code so
  their variables persist across a session. Only leading imports are
  rewritten, and only forms eryx fully understands; anything else (an import
  after other statements, import attributes) is QuickJS's syntax error.
  Running the same import again in a session is fine. A session snapshot
  records these imports rather than the modules, and restoring it imports
  them again (so the restoring sandbox needs the same environment); a
  variable reassigned since its import keeps its new value. Only imports
  from executes that succeeded are recorded: if an execute throws after its
  imports ran, run those imports again after a restore.
- **`await import("…")`** works anywhere.
- **`require("…")`** imports synchronously, for CommonJS-style code. A module
  that can't finish without awaiting (top-level await on something
  asynchronous) can't be required; use `await import()`.
- **CommonJS packages** (lodash, say) are bundled as a module whose default
  export is `module.exports`. Named imports of its properties work too:
  `import { chunk } from "lodash"`.
- Only what the environment exports is importable, plus the shared chunks its
  modules import from each other. Anything else fails with "Cannot find
  module", naming what the environment provides.
- **Node built-ins** (`node:fs`, or a bare `fs` that no package provides)
  aren't available yet: importing one throws "node:fs isn't available in eryx
  yet". Packages written for browsers or Workers generally avoid them.

## What isn't supported yet

- Python and shell environments (`build` returns `Unsupported`).
- Node's built-in modules, and web APIs QuickJS lacks (`crypto`,
  `TextEncoder`, timers). Packages that need them fail when they use them.
- Packages with native addons or WebAssembly.
