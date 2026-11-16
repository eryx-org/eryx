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
                                 # (with `env build`; see "Building it")

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
A definition can also be built in code, without TOML:
`EnvironmentDefinition::new("data-tools", Language::JavaScript)?.with_npm(NpmSection::new(["zod@^4"]))`.
The `packages` feature embeds the bundler if it's there at build time
(`ERYX_BUNDLER_CWASM`, the setup cache, or the workspace). Otherwise, as in a
crates.io build or a released `eryx-precompile`, it's loaded at run time from
the cache `eryx-precompile setup --bundler` writes, checked against its
checksum; without one, the build fails saying to run that.

Where the lockfile goes: `env build` keeps it next to the definition (its
`base_dir`). Through the library, `BuildOptions` without a `base_dir` keeps it
in the eryx cache (`<cache>/packages/lockfiles/`) rather than the current
directory. A lockfile there isn't shared across machines, so two machines can
resolve different versions, and definitions with the same name share it. For
reproducible builds, commit the lockfile next to the definition, point
`base_dir` at that directory, and build with `frozen`.

> With a released `eryx-precompile`, run `eryx-precompile setup --bundler`
> once before the first `env build`.

The result is a directory with a manifest (`eryx-environment.json`: language,
inputs, what it exports, and a hash of everything sandboxes rely on), the
lockfile and the modules. Rebuilding from the same lockfile gives the same
hash.

## Packing installed packages

When the packages are already installed (by `npm ci`, pnpm, or a service
that resolves and fetches them), `env pack` turns them into an environment
without resolving or fetching anything:

```bash
npm ci --ignore-scripts --omit=dev
eryx-precompile env pack . --name data-tools --entry date-fns/format \
    --lockfile package-lock.json --out envs/data-tools
```

- **Entries:** what's importable. Without `--entry`, the tree's
  `package.json` `dependencies`, skipping (with a warning in the manifest)
  any with nothing to import, such as `@types/*` or CLI-only packages; with
  it, exactly the entries given.
- **Layouts:** npm's, and pnpm's isolated one (`node_modules/.pnpm`), work
  as installed, symlinks included. Packages are looked for in `node_modules`
  from the tree up to `--root` (default: the tree itself), as Node does, so
  for a workspace member with hoisted dependencies, workspace packages or
  `file:` dependencies outside the tree, pass a `--root` that holds them (a
  monorepo's root, say). Every package symlink must be relative and lead
  inside the root. An absolute symlink, or one that leads outside the root,
  fails before bundling, naming the link. Links that aren't packages
  (`node_modules/.bin`) and links that lead nowhere are left alone. Yarn's
  Plug'n'Play has no `node_modules`, so use its `nodeLinker: node-modules`.
- **Safety:** the bundler parses the packages in a sandbox. What it can
  read, and so what can end up in the environment, is exactly:
  - the contents of the `node_modules` dirs from the tree up to the root;
  - the whole directory each package symlink points at (a workspace
    member's or `file:` package's directory, everything in it).

  Nothing else of the tree or the root is visible: a package that imports
  the app's own files (a `../../service-account.json`), or a workspace
  member that imports a file beside its directory, fails the pack instead.
  (The bundler reads a mirror of just those paths, hard-linked next to the
  output, so links resolve from packages' real paths as Node's do.)
- **Unresolved imports fail the pack:** an import the installed packages
  don't provide is an error listing each one, rather than an environment
  that fails when imported. Two kinds stay external, with a warning in the
  manifest, so they fail only if the code imports them at run time: one the
  importing package declares optional (`optionalDependencies`, or an
  optional peer in `peerDependenciesMeta`), which `npm ci --omit=dev`
  leaves out, and one passed to `--allow-unresolved` (repeatable). A
  `require` in a `try`/`catch`, the usual way to use an optional
  dependency (`ws` and `bufferutil`), is left external without a warning.
  Nothing is fetched, and pass `--ignore-scripts` to the install so no
  package code runs there either.
- `--lockfile` copies the given file into the environment and records it
  and its hash in the manifest's `inputs`, with the installed versions of
  the entries' packages. None of that is part of the hash: packing the same
  installed tree again gives the same environment.

In Rust, the same is `eryx::environment::pack(tree, out, &PackOptions::new(name, Language::JavaScript))`,
with the `packages` feature. The directory format is described in
[Environment Format](../api/environment-format.md).

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
  variable reassigned since its import keeps its new value. Values that hold
  a class instance (a `Buffer`, an `EventEmitter`, a `TextDecoder`) can't be
  kept with their class, so they're left out and named in the snapshot's
  `skipped()`, like functions. Only imports
  from executes that succeeded are recorded: if an execute throws after its
  imports ran, run those imports again after a restore.
- **`await import("…")`** works anywhere.
- **`require("…")`** imports synchronously, for CommonJS-style code. A module
  that can't finish without awaiting (top-level await on something
  asynchronous) can't be required; use `await import()`. `require` settles
  the import by running queued promise jobs, so pending callbacks may run
  earlier than they otherwise would.
- **CommonJS packages** (lodash, say) are bundled as a module whose default
  export is `module.exports`. Named imports of its properties work too:
  `import { chunk } from "lodash"`.
- Only what the environment exports is importable, plus the shared chunks its
  modules import from each other. Anything else fails with "Cannot find
  module", naming what the environment provides.
- **Node built-ins:** `node:buffer`, `node:events`, `node:path`,
  `node:string_decoder` (the npm polyfills, vendored), `node:util` (a subset:
  `format`, `inspect`, `inherits`, `promisify`, `types`, ...) and
  `node:crypto` (random values only: `randomBytes`, `randomUUID`,
  `randomInt`, `getRandomValues`) and `node:url` (`URL`, `URLSearchParams`,
  `fileURLToPath`, `pathToFileURL`, `urlToHttpOptions`, `domainToASCII`,
  `domainToUnicode`, and a subset of the legacy `parse`, `format` and
  `resolve`) work, with or without `node:`, and with
  `require` too. They don't need an environment. The rest (`fs`, `net`,
  `child_process`, `http`, `os`, ...) throw "node:fs isn't available in eryx
  yet" on import, so a package that imports one statically fails when it's
  imported. Packages written for browsers or Workers generally avoid them.
- **Web globals:** `crypto.getRandomValues()` and `crypto.randomUUID()`
  (from the host's random source, so uuid and nanoid work), `TextEncoder`
  and `TextDecoder` (UTF-8 only), and timers (`setTimeout` and friends, with
  `node:timers` and `node:timers/promises`; see
  [Timers in JavaScript](./sandboxes.md#timers-in-javascript)).
- **`URL` and `URLSearchParams`** follow the WHATWG URL standard (core-js's
  implementation), with one gap: a non-ASCII host is lowercased and
  punycode-encoded, without the full IDNA mapping a browser applies.
  `fetch` takes a `URL`, but checks its `href` with its own stricter parser,
  so a URL `fetch` can't use (credentials, a scheme other than `http`/`https`)
  is refused as before. A `URL` kept in session state is a class instance, so
  snapshots skip it; keep its `href` instead.
- **Imports are checked like ES modules:** importing a name a module doesn't
  export (or a default from a module without one) throws a SyntaxError
  naming the export and the module.

## What isn't supported yet

- Python and shell environments (`build` returns `Unsupported`).
- The other Node built-ins (above), `crypto.subtle` and `node:crypto`'s
  hashes and ciphers. A package that imports a missing
  built-in fails when it's imported; one that uses a missing API fails when
  it calls it.
- Node's globals: there's no global `Buffer` or `process.nextTick`. Code
  that uses `Buffer` without importing it (common in packages written for
  Node) throws `ReferenceError: Buffer is not defined`; `import { Buffer }
  from "node:buffer"` works.
- Packages with native addons or WebAssembly.
