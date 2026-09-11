# Pre-compiling Runtimes

The `eryx-precompile` tool generates platform-specific pre-compiled runtimes that dramatically speed up sandbox creation. Instead of compiling WebAssembly at startup, sandboxes load pre-compiled native code directly.

## Installation

Install from GitHub Releases using `cargo binstall`:

```bash
cargo binstall eryx-precompile
```

Or download a binary directly from the [GitHub Releases page](https://github.com/eryx-org/eryx/releases).

## Quick Setup

For most users, the `setup` subcommand handles everything:

```bash
eryx-precompile setup
```

This will:

1. Download `runtime.wasm` from the matching GitHub Release
2. Pre-compile it to native code for your platform
3. Cache the result in `~/.cache/eryx/`

After this one-time setup, `cargo build` with `features = ["embedded"]` finds the cached runtime automatically. You can also point to a specific file:

```bash
export ERYX_RUNTIME_CWASM=/path/to/runtime.cwasm
```

### Setup Options

```bash
# Use a specific version (defaults to the installed binary's version)
eryx-precompile setup --version 0.4.5

# Verbose output for debugging
eryx-precompile setup --verbose
```

## Advanced: The `compile` Subcommand

For cross-compilation, custom CPU targets, or bundling packages into the runtime, use `compile`:

```bash
eryx-precompile compile <input.wasm> [OPTIONS]
```

### Pre-initialization

Pre-initialization runs Python's interpreter startup and captures the memory state, reducing session creation time from ~450ms to ~1-5ms:

```bash
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib
```

To produce an architecture-independent `.wasm` (for later compilation on different targets):

```bash
eryx-precompile compile runtime.wasm -o runtime-preinit.wasm \
  --preinit --stdlib ./python-stdlib --wasm-only
```

### CPU Target Selection

Control which CPU features the compiled runtime uses:

```bash
# Use host CPU features (default, fastest but not portable)
eryx-precompile compile runtime.wasm -o runtime.cwasm --target native

# AVX2/FMA — recommended for cloud VMs (Fly.io, AWS, GCP)
eryx-precompile compile runtime.wasm -o runtime.cwasm --target x86-64-v3

# Broad compatibility (~2008+ x86 CPUs)
eryx-precompile compile runtime.wasm -o runtime.cwasm --target x86-64-v2

# Baseline x86 (maximum compatibility)
eryx-precompile compile runtime.wasm -o runtime.cwasm --target x86-64

# Full target triple for cross-compilation
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --target aarch64-unknown-linux-gnu
```

| Target | CPU Features | Use Case |
|--------|-------------|----------|
| `native` | Host CPU (default) | Local development |
| `x86-64-v3` | AVX2, FMA, BMI1/2 | Cloud VMs (no AVX-512) |
| `x86-64-v2` | SSE4.2, POPCNT | Older servers |
| `x86-64` | Baseline SSE2 | Maximum compatibility |
| `<triple>` | Native for target arch | Cross-compilation |

### Bundling Packages

Bundle Python packages into the pre-compiled runtime so they're instantly available without installation at runtime:

```bash
# Bundle a single package
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package numpy-2.2.3-wasi.tar.gz --import numpy

# Bundle multiple packages
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package jinja2-3.1.2-py3-none-any.whl \
  --package markupsafe-2.1.3-py3-none-any.whl \
  --import jinja2

# Use a site-packages directory
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --site-packages ./my-site-packages --import jinja2
```

Supported package formats: `.whl`, `.tar.gz`, and directories. Native extensions (`.so` files) are detected and linked automatically.

### Setup Code

Beyond importing modules, you can execute arbitrary Python code during pre-initialization. The resulting state (variables, objects, environments) is captured in the memory snapshot — each sandbox starts with that state in copy-on-write memory, preserving full isolation.

This is useful when your sandboxes all need the same expensive setup that doesn't change between requests:

```bash
# Bake a Jinja2 SandboxedEnvironment with custom filters into the snapshot
eryx-precompile compile runtime.wasm -o jinja2.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package jinja2.whl --package markupsafe.whl \
  --import jinja2 --import jinja2.sandbox \
  --setup-code "
from jinja2.sandbox import SandboxedEnvironment
env = SandboxedEnvironment()
"

# For longer setup scripts, use --setup-file
eryx-precompile compile runtime.wasm -o jinja2.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package jinja2.whl --package markupsafe.whl \
  --import jinja2 \
  --setup-file setup.py
```

Each sandbox created from the resulting artifact starts with `env` already defined. Per-request code only needs to handle the request-specific work (deserializing data, compiling the template, rendering).

### Callbacks

Registering callbacks on a sandbox installs a Python wrapper for each of them inside the guest. A persistent session pays for that once, but a fresh sandbox per request starts from the snapshot, and the snapshot only knows the callbacks it was taken with — none, by default — so every request re-runs the installation (a few milliseconds of Python for a handful of callbacks, more than the instantiation itself).

If the set of callbacks is fixed per deployment, declare it at pre-initialization and the wrappers are part of the snapshot. A sandbox that registers callbacks with the same names, descriptions and parameter schemas (in any order) then skips the installation; one that registers a different set still works, it just installs as before.

```bash
# callbacks.json: what the sandboxes will register at runtime
# [
#   {"name": "get_time", "description": "Current time", "parameters": {"type": "object", "properties": {}}},
#   {"name": "fetch", "description": "HTTP GET", "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}
# ]
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --callbacks callbacks.json
```

`parameters` defaults to `{}` when omitted. Setup code cannot invoke the callbacks: there is no host to answer them during pre-initialization. In Rust, pass `PreInitOptions::callbacks` (build the declarations with `eryx::preinit::callback_declaration`); in Python, pass `callbacks=` to `SandboxFactory`.

### Verification

By default, `compile` verifies the output by creating a test sandbox. You can add custom verification:

```bash
# Verify that numpy actually works, not just imports
eryx-precompile compile runtime.wasm -o numpy.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package numpy-wasi.tar.gz --import numpy \
  --verify-code "import numpy; print(numpy.array([1,2,3]).sum())"

# Skip verification (faster, use when you know the input is good)
eryx-precompile compile runtime.wasm -o runtime.cwasm --no-verify
```

## Common Recipes

### Local Development (Rust)

```bash
# One-time setup
cargo binstall eryx-precompile
eryx-precompile setup

# Then in your project
cargo build --features embedded
```

### Fly.io Deployment

```bash
# Pre-compile for x86-64-v3 (matches Fly.io hardware)
eryx-precompile compile runtime.wasm -o runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --target x86-64-v3
```

### Two-stage Cross-compilation

Create an architecture-independent snapshot, then compile for each target:

```bash
# Stage 1: Pre-initialize (architecture-independent)
eryx-precompile compile runtime.wasm -o runtime-preinit.wasm \
  --preinit --stdlib ./python-stdlib --wasm-only

# Stage 2: AOT compile for each target
eryx-precompile compile runtime-preinit.wasm -o runtime-x86.cwasm \
  --target x86-64-v3
eryx-precompile compile runtime-preinit.wasm -o runtime-arm.cwasm \
  --target aarch64-unknown-linux-gnu
```

### Custom Runtime with Packages

```bash
# Build a runtime with numpy baked in
eryx-precompile compile runtime.wasm -o numpy-runtime.cwasm \
  --preinit --stdlib ./python-stdlib \
  --package numpy-2.2.3-wasi.tar.gz \
  --import numpy \
  --verify-code "import numpy; print(numpy.zeros((2,2)))"
```

## Environment Variables for CPU Targeting

When using eryx as a library — from Python (`pyeryx`), Rust (`Sandbox::embedded()`), or JavaScript — the embedded runtime is compiled to native code at build time. By default it targets the **host CPU**, which may include instructions (e.g. AVX-512) that aren't available on the machine where your application actually runs.

If the build machine has different CPU features from the deployment target, the sandbox will fail to initialize at runtime with an error like:

```text
Compilation settings are not compatible with the native host
```

Set these environment variables at **build time** (when the `.cwasm` is generated) or at **import time** (when the library first loads its embedded runtime) to control which CPU features the compiled code uses:

### `ERYX_CPU_FEATURES`

Choose an x86-64 microarchitecture level:

| Value | CPU Features | Use Case |
|-------|-------------|----------|
| `native` | Host CPU (default) | Local development |
| `x86-64-v3` | AVX2, FMA, BMI1/2 | **Recommended for cloud VMs** (Fly.io, AWS, GCP) |
| `x86-64-v2` | SSE4.2, POPCNT, SSSE3 | Older servers (~2008+) |
| `x86-64` | Baseline SSE2 only | Maximum compatibility |
| `x86-64-v4` | AVX-512 | Skylake-X and newer |

Example with Python:

```bash
ERYX_CPU_FEATURES=x86-64-v3 python my_app.py
```

Example with Rust:

```bash
ERYX_CPU_FEATURES=x86-64-v3 cargo build --features embedded --release
```

### `ERYX_TARGET`

For cross-compilation, specify a full target triple:

```bash
ERYX_TARGET=aarch64-unknown-linux-gnu cargo build --features embedded --release
```

### `ERYX_CRANELIFT_FLAGS`

For fine-grained control over individual Cranelift compiler flags:

```bash
ERYX_CRANELIFT_FLAGS=has_avx512f=false,has_avx512bw=false cargo build --features embedded --release
```

> **Tip:** For most deployments, `ERYX_CPU_FEATURES=x86-64-v3` is the right choice. It covers all cloud VMs from the last decade while avoiding AVX-512 instructions that many virtualized environments don't expose.

## Cache Location

The `setup` command caches compiled runtimes at:

| Platform | Path |
|----------|------|
| Linux/macOS | `~/.cache/eryx/` (or `$XDG_CACHE_HOME/eryx/`) |
| Windows | `%USERPROFILE%\.cache\eryx\` |

Files are named `runtime-v{version}-{arch}-{os}.cwasm`. To force a re-download, delete the cached file and run `setup` again.

## Next Steps

- [Installation](../getting-started/installation.md) - Setting up the `embedded` feature
- [Packages](./packages.md) - Using Python packages in sandboxes
- [Sandboxes](./sandboxes.md) - Creating and configuring sandboxes
