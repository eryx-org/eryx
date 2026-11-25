# Packages

Eryx supports installing Python packages in sandboxes. This allows sandboxed code to use third-party libraries like `requests`, `jinja2`, `numpy`, and more.

## Supported Package Formats

Eryx supports two package formats:

| Format | Extension         | Description                                |
| ------ | ----------------- | ------------------------------------------ |
| Wheel  | `.whl`            | Standard Python wheel format (recommended) |
| Tar.gz | `.tar.gz`, `.tgz` | Compressed archive format                  |

## Adding Packages with SandboxFactory

The recommended way to use packages is with `SandboxFactory`, which pre-installs packages once and creates fast sandbox instances:

```python
import eryx

# Create a factory with packages pre-installed
factory = eryx.SandboxFactory(
    packages=[
        "path/to/jinja2-3.1.2-py3-none-any.whl",
        "path/to/markupsafe-2.1.3-py3-none-any.whl",
    ],
    imports=["jinja2"],  # Pre-import these modules during initialization
)

# Create sandboxes quickly - packages are already installed
sandbox = factory.create_sandbox()
result = sandbox.execute("""
from jinja2 import Template
template = Template("Hello {{ name }}!")
print(template.render(name="World"))
""")
print(result.stdout_text)  # "Hello World!"
```

## Adding Packages to a Sandbox Builder (Rust)

In Rust, use the sandbox builder to add packages:

```rust,ignore
# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_package("path/to/jinja2-3.1.2-py3-none-any.whl")?
        .with_package("path/to/markupsafe-2.1.3-py3-none-any.whl")?
        .build()?;

    let result = sandbox.execute(r#"
from jinja2 import Template
template = Template("Hello {{ name }}!")
print(template.render(name="Rust"))
    "#).await?;

    println!("{}", result.stdout_text());

    Ok(())
}
```

## Pre-importing Modules

Pre-importing modules during factory initialization speeds up sandbox creation:

```python
import eryx

# Without pre-import: first import in sandbox is slow
factory_without = eryx.SandboxFactory(
    packages=["path/to/jinja2.whl", "path/to/markupsafe.whl"],
)

# With pre-import: imports are already done
factory_with = eryx.SandboxFactory(
    packages=["path/to/jinja2.whl", "path/to/markupsafe.whl"],
    imports=["jinja2"],  # Import during initialization
)

# Sandboxes from factory_with start faster
sandbox = factory_with.create_sandbox()
```

## Finding WASI-Compatible Packages

Eryx runs Python in WebAssembly, which means packages with native extensions need to be compiled for WASI. Options include:

### Pure Python Packages

Pure Python packages (no native extensions) work out of the box:

- `requests`
- `jinja2`
- `pyyaml`
- `httpx`
- `beautifulsoup4`

### Prebuilt Native Packages

Packages with native code (C, C++, Rust), such as `numpy` and `pandas`, need builds for eryx's WASI: their wheels for Linux or macOS won't load. We publish WASI builds of some at a public pip index, built from the recipes in [eryx-recipes](https://github.com/eryx-org/eryx-recipes/tree/main/wheels): `numpy`, `pandas`, `pydantic-core`, `rapidfuzz`, `markupsafe` and `ujson` so far, and [the index](https://wheels.eryx.run/cp314-wasip2-sdk33/simple/) lists them all.

Download them with pip, for the WASI platform. Pure-Python dependencies come from PyPI as usual:

```bash
pip download --dest wheels --only-binary :all: \
    --python-version 3.14 --implementation cp --abi cp314 \
    --platform wasi_0_0_0_wasm32 --platform any \
    --extra-index-url https://wheels.eryx.run/cp314-wasip2-sdk33/simple/ \
    pandas
```

Then add the wheels as packages, as above:

```python
import glob
import eryx

factory = eryx.SandboxFactory(
    packages=sorted(glob.glob("wheels/*.whl")),
    imports=["pandas"],
)
```

#### With uv

To keep the packages in a `pyproject.toml` and lock them, point uv at the index for the packages with native code:

```toml
[project]
name = "sandbox-packages"
version = "0"
requires-python = "==3.14.*"
# List every package with native code here, even one that's only a
# dependency of another (numpy for pandas, markupsafe for jinja2): uv only
# applies tool.uv.sources to direct dependencies, and takes the rest from PyPI.
dependencies = ["pandas", "numpy", "jinja2", "markupsafe"]

[tool.uv]
package = false

[[tool.uv.index]]
name = "eryx-wasi"
url = "https://wheels.eryx.run/cp314-wasip2-sdk33/simple/"
explicit = true

[tool.uv.sources]
numpy = { index = "eryx-wasi" }
pandas = { index = "eryx-wasi" }
markupsafe = { index = "eryx-wasi" }
```

uv locks for WASI fine but can't install for it, so export the lock and let pip download the locked wheels:

```bash
uv lock
uv export --format requirements-txt --no-emit-project -o requirements.txt
pip download --dest wheels --no-deps --require-hashes --only-binary :all: \
    --python-version 3.14 --implementation cp --abi cp314 \
    --platform wasi_0_0_0_wasm32 --platform any \
    --extra-index-url https://wheels.eryx.run/cp314-wasip2-sdk33/simple/ \
    -r requirements.txt
```

A package whose native wheel isn't there fails `pip download` (it finds only a Linux or macOS wheel): give it a source on the `eryx-wasi` index. If one of our builds is rebuilt at the same version (eryx changed what it links to), its hash changes: `uv lock --upgrade-package <name>` picks up the new file.

In Rust, native extensions need the `native-extensions` feature (see [Installation](../getting-started/installation.md)). `eryx-precompile compile --preinit --package <wheel> ...` bakes them into a [precompiled runtime](./precompile.md).

The index's path, `cp314-wasip2-sdk33`, is what the wheels need: CPython 3.14 on eryx's WASI (`wasip2`), built with wasi-sdk 33, which every eryx since 0.6 uses. Their extension modules are shared libraries linked against eryx's own `libpython`, so they load in eryx (and other runtimes that link Python modules the same way, like componentize-py), not in CPython's own WASI build. If eryx moves to another wasi-sdk, the builds for it get a new path.

To ask for another package, [open an issue](https://github.com/eryx-org/eryx-recipes/issues) in eryx-recipes, or add a recipe (its `wheels/README.md` says how).

### Other WASI Builds

Some other projects publish WASI builds too, mostly for [componentize-py](https://github.com/bytecodealliance/componentize-py). Check that they're built for the same CPython and wasi-sdk before using them:

- [dicej/wasi-wheels](https://github.com/dicej/wasi-wheels/)
- [benbrandt/wasi-wheels](https://github.com/benbrandt/wasi-wheels/) (see also [this PR](https://github.com/benbrandt/wasi-wheels/pull/272) for an example of adding a new wheel)

## Baking Callbacks into the Factory

Registering callbacks installs a Python wrapper for each of them inside the guest. A fresh sandbox starts from the factory's snapshot, and the snapshot only knows the callbacks it was created with — none, by default — so every `create_sandbox()` re-runs that installation on its first execution (a few milliseconds for a handful of callbacks, more than creating the sandbox itself).

If every sandbox from a factory registers the same callbacks, pass them to the factory instead. Their declarations (name, description, parameter schema) are baked into the snapshot, and `create_sandbox()` / `create_session()` register them unless given callbacks of their own:

```python
import eryx

def get_time():
    import time
    return {"timestamp": time.time()}

factory = eryx.SandboxFactory(
    callbacks=[{"name": "get_time", "fn": get_time, "description": "Returns current time"}],
)

sandbox = factory.create_sandbox()  # get_time() is ready; no per-sandbox setup
result = sandbox.execute("print((await get_time())['timestamp'] > 0)")
```

A sandbox that is given a different set of callbacks still works; it installs them itself as before. Setup code cannot invoke the callbacks, since nothing answers them while the snapshot is being built. A saved factory keeps the baked declarations but not the Python functions, so pass the same `callbacks=` to `SandboxFactory.load()`.

## Package Dependencies

When installing packages, you must include all dependencies:

```python
import eryx

# requests requires urllib3, certifi, idna, charset-normalizer
factory = eryx.SandboxFactory(
    packages=[
        "requests-2.31.0-py3-none-any.whl",
        "urllib3-2.1.0-py3-none-any.whl",
        "certifi-2024.2.2-py3-none-any.whl",
        "idna-3.6-py3-none-any.whl",
        "charset_normalizer-3.3.2-py3-none-any.whl",
    ],
    imports=["requests"],
)
```

## Error Handling

Package installation errors are caught at factory/sandbox creation time:

```python
import eryx

try:
    factory = eryx.SandboxFactory(
        packages=["nonexistent-package.whl"],
    )
except FileNotFoundError as e:
    print(f"Package not found: {e}")

try:
    factory = eryx.SandboxFactory(
        packages=["file.unknown"],  # Unknown format
    )
except ValueError as e:
    print(f"Invalid format: {e}")
```

## Caching Compiled Components

Eryx caches compiled WebAssembly components to speed up repeated sandbox creation:

## Saving and Loading Factories

Save pre-configured factories for fast loading:

```python
import eryx
from pathlib import Path

# Create factory with packages
factory = eryx.SandboxFactory(
    packages=["jinja2.whl", "markupsafe.whl"],
    imports=["jinja2"],
)

# Save to disk
factory.save(Path("jinja2-factory.bin"))

# Later: load quickly
loaded_factory = eryx.SandboxFactory.load(Path("jinja2-factory.bin"))
sandbox = loaded_factory.create_sandbox()
```

## Standard Library Availability

The Python standard library is automatically available:

```python
import eryx

sandbox = eryx.Sandbox()
result = sandbox.execute("""
import json
import base64
import hashlib
import re
import datetime

data = json.dumps({"key": "value"})
encoded = base64.b64encode(b"hello").decode()
hash_val = hashlib.md5(b"test").hexdigest()[:8]
match = re.search(r'\\d+', '123abc')

print(f"json: {data}")
print(f"base64: {encoded}")
print(f"hash: {hash_val}")
print(f"regex: {match.group()}")
""")
print(result.stdout_text)
```

## Common Package Examples

### Jinja2 (Templating)

```python
import eryx

factory = eryx.SandboxFactory(
    packages=["jinja2.whl", "markupsafe.whl"],
    imports=["jinja2"],
)

sandbox = factory.create_sandbox()
result = sandbox.execute("""
from jinja2 import Template
t = Template("{% for item in items %}{{ item }}{% endfor %}")
print(t.render(items=[1, 2, 3]))
""")
print(result.stdout_text)  # "123"
```

### Requests (HTTP Client)

```python
import eryx

factory = eryx.SandboxFactory(
    packages=[
        "requests.whl",
        "urllib3.whl",
        "certifi.whl",
        "idna.whl",
        "charset_normalizer.whl",
    ],
    imports=["requests"],
)

config = eryx.NetConfig.permissive()
sandbox = factory.create_sandbox(network=config)

result = sandbox.execute("""
import requests
r = requests.get("https://httpbin.org/get", timeout=10)
print(f"Status: {r.status_code}")
""")
```

### PyYAML (YAML Parser)

```python
import eryx

factory = eryx.SandboxFactory(
    packages=["pyyaml.whl"],
    imports=["yaml"],
)

sandbox = factory.create_sandbox()
result = sandbox.execute("""
import yaml

data = yaml.safe_load('''
name: John
age: 30
items:
  - apple
  - banana
''')
print(f"Name: {data['name']}, Items: {data['items']}")
""")
```

## Best Practices

1. **Use SandboxFactory** - Pre-install packages once, create sandboxes quickly
2. **Pre-import common modules** - Speeds up sandbox initialization
3. **Include all dependencies** - Packages don't auto-resolve dependencies
4. **Cache compiled components** - Use filesystem cache for production
5. **Save/load factories** - Avoid repeated initialization costs
6. **Prefer pure Python packages** - They work without WASI compilation

## Next Steps

- [Sandboxes](./sandboxes.md) - Creating and configuring sandboxes
- [SandboxFactory](./sandboxes.md#sandboxfactory-for-fast-creation) - Factory pattern details
- [Networking](./networking.md) - Enable network for HTTP packages
