# Environments

An environment is a set of packages installed and packed ahead of time, in CI
or a deploy step, into a directory that sandboxes mount read-only. Sandboxes
never see package names or a registry: they import what the environment
provides. JavaScript is supported today (npm packages); Python and the shell
will use the same format and the same `with_environment`.

## Getting the packages

eryx doesn't resolve or fetch packages. Install them with your package
manager, or let a service do it:

- **npm or pnpm:** `npm ci --ignore-scripts --omit=dev` (or `pnpm install
  --prod --ignore-scripts`) from a committed lockfile, then pack the tree as
  below.
- **eryx-run:** the hosted service resolves and fetches packages from a
  definition (package specs, a registry, a minimum release age, a lockfile)
  and packs them with eryx, so its environments are the same format.

## Packing them

`env pack` turns the installed packages into an environment, without
resolving or fetching anything:

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
with the `env-pack` feature. The directory format is described in
[Environment Format](../api/environment-format.md).

The `env-pack` feature embeds the bundler if it's there at build time
(`ERYX_BUNDLER_CWASM`, the setup cache, or the workspace). Otherwise, as in a
crates.io build or a released `eryx-precompile`, it's loaded at run time from
the cache `eryx-precompile setup --bundler` writes, checked against its
checksum; without one, packing fails saying to run that.

> With a released `eryx-precompile`, run `eryx-precompile setup --bundler`
> once before the first `env pack`.

The result is a directory with a manifest (`eryx-environment.json`: language,
inputs, what it exports, and a hash of everything sandboxes rely on), the
lockfile if given, and the modules.

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

Loading and mounting only need eryx's `environment` feature, which doesn't
include the bundler. The sandbox builder's `build()` refuses an environment
for another language, and checks the environment still hashes to what it did
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
  `resolve`) and `node:fs` (a subset: whole-file reads and writes,
  directories, `stat`, rename, copy and remove, sync, with callbacks and as
  `fs/promises`, on the sandbox's files) work, with or without `node:`, and
  with `require` too. They don't need an environment. The rest (`net`,
  `child_process`, `http`, `os`, ...) throw "node:net isn't available in eryx
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

- Python and shell environments (`pack` returns `Unsupported`).
- The other Node built-ins (above), `crypto.subtle` and `node:crypto`'s
  hashes and ciphers. A package that imports a missing
  built-in fails when it's imported; one that uses a missing API fails when
  it calls it.
- Node's globals: there's no global `Buffer` or `process.nextTick`. Code
  that uses `Buffer` without importing it (common in packages written for
  Node) throws `ReferenceError: Buffer is not defined`; `import { Buffer }
  from "node:buffer"` works.
- Packages with native addons or WebAssembly.
