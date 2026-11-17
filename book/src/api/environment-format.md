# Environment Format

An environment is a directory that eryx's `pack` writes and
`Environment::load` reads. The two may be different eryx versions, for
example when a service packs environments and servers of another version
mount them. This page is the contract between them.

## Layout

```text
<environment>/
  eryx-environment.json   the manifest
  <lockfile>              optional: what was installed (package-lock.json, pnpm-lock.yaml)
  modules/                the content directory: what sandboxes import
```

Sandboxes see the directory read-only at `/eryx/environment`.

## The manifest

`eryx-environment.json` is a JSON object. Fields marked *identity* are covered
by the hash, so changing any of them changes the environment. *Informational*
fields aren't: they record how it was made.

| Field | Kind | Meaning |
|---|---|---|
| `format` | identity | Always `"eryx-environment"` |
| `formatVersion` | identity | The format version, `1` today |
| `name` | identity | The environment's name |
| `language` | identity | `"javascript"`, `"python"` or `"shell"`. A sandbox for another language refuses it |
| `content.dir` | identity | The content directory, relative to the root (`"modules"`) |
| `exports` | identity | What's importable: specifier → file, relative to the root and inside `content.dir` (`"date-fns/format": "modules/date-fns__format.js"`) |
| *the files in `content.dir`* | identity | Every regular file's path and bytes |
| `content.hash` | — | The identity hash, below. Pin it with `Environment::load_expecting` |
| `content.files`, `content.bytes` | informational | The content directory's file count and size |
| `inputs` | informational | `packages`, and optionally `registry`, `lockfile` (its file name in the environment) and `lockfileHash` |
| `builder` | informational | `eryx` (the version that wrote it) and `tools` (e.g. `"bundler": "rolldown 1.3.2"`) |
| `warnings` | informational | What the build warned about, if anything |

Every informational field is optional: a writer may leave out `inputs`,
`builder`, `warnings`, `content.files` or `content.bytes` (or any field
inside `inputs` and `builder`), and readers fill in empty values.

Paths use `/`, are relative, and must stay inside the environment. The
content directory may only hold regular files and directories: a symlink is
refused.

## The identity hash

`content.hash` is `sha256-<hex>` of, in order:

1. Each of `"eryx-environment"`, the `formatVersion` in decimal, `name`,
   `language` and `content.dir`, as UTF-8, each followed by a `0` byte.
2. For each export in byte order of its specifier: the specifier, a `0`
   byte, its path, a `0` byte.
3. `files`, a `0` byte, then the *content hash* as a `sha256-<hex>` string.

The content hash is SHA-256 over every regular file under `content.dir`, in
byte order of its `/`-separated path relative to `content.dir`. For each
file it takes the path, a `0` byte, the file's length as a little-endian
`u64`, and its bytes.

`Environment::load` recomputes it, and refuses an environment whose files or
identity fields no longer match. eryx's tests pin the hash of a fixed
environment, so it can't change by accident.

## What the JavaScript guest reads

The guest reads only `exports` and `content.hash` from the manifest. A bare
specifier in `exports` is importable, and its modules import each other's
shared chunks by relative path. How chunks are named isn't part of the
format: everything goes through `exports`.

## Versioning

- `formatVersion` changes when an identity field, the hash, or anything the
  loader needs changes. A reader refuses a version it doesn't know, naming
  both versions.
- New informational fields are added without a new version. Readers ignore
  fields they don't know.
- A new format version is read before it's written: an eryx release that
  reads both the old and the new version ships first, and builders move to
  writing the new one after the servers that load their environments have
  it. `builder.eryx` says which eryx wrote an environment.
