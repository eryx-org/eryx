# GPL-licensed CLI recipes

Recipes here build commands whose source is GPL-licensed, kept apart from the
rest of `clis/` so a licence audit can tell them apart at a glance (#837).

- Each subdirectory is one command. Everything in it (the manifest, patches,
  shims and scripts) is under that command's licence, given in its
  `NOTICE.md`.
- The components they build are separate artifacts. They are never linked
  into or embedded in the eryx crates, the `eryx` binary or any other
  MIT/Apache-licensed artifact; a sandbox loads them at run time like any
  other command.

Build one with `mise run build-cli -- clis/gpl/<name>/<name>.toml`.
