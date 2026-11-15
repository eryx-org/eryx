# eryx-packages

Resolve, fetch and cache packages for [eryx](https://github.com/eryx-org/eryx)
sandboxes, ahead of time and on the host: sandboxes never talk to a registry.

- `environment`: the hand-written environment definition (TOML) and the
  manifest of a built environment, language-neutral.
- `npm`: resolve npm packages to a lockfile and install them as a
  `node_modules` tree (integrity-checked, no install scripts, minimum release
  age, size limits, offline and frozen modes).

Building an environment (installing, then bundling for JavaScript) is
`eryx::environment::build`, with eryx's `packages` feature, or
`eryx-precompile env build <definition.toml> --out <dir>`.
