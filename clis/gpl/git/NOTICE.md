# git: licence notice

git is licensed under the GNU General Public License, version 2 only
(GPL-2.0-only); see `COPYING` in its source, which `git.toml` pins.

Everything in this directory is distributed under the same licence,
GPL-2.0-only: `git.toml`, `patches/` (including `git-wasi.patch`, our
modifications to git) and `shims/`. It is not covered by eryx's MIT/Apache-2.0
licence.

- **Source:** the git release `git.toml` pins (repository, tag and commit),
  plus the patches and shims here, are the complete corresponding source of
  the component `mise run build-cli -- clis/gpl/git/git.toml` builds. zlib
  (Zlib licence) is fetched and built as `git.toml`'s dependency.
- **Separation:** the git component is its own artifact. It is not linked
  into, embedded in or shipped with the eryx crates or binaries; a sandbox
  loads it at run time, and it talks to eryx only through the
  `eryx:process/run` interface, as any command does.
- **Open questions:** whether and how eryx ships GPL components at all
  (artifacts, notices, an SBOM) is tracked in #837, and needs a licensing
  review before any release includes one.
