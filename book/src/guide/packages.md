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
print(result.stdout)  # "Hello World!"
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

    println!("{}", result.stdout);

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

### WASI-Compiled Packages

Some packages with native extensions have WASI-compiled versions available:

- Check [pypi.org](https://pypi.org) for wheels with `wasi` in the platform tag
- Multiple `wasi-wheels` repositories exist, including:
  - [dicej/wasi-wheels](https://github.com/dicej/wasi-wheels/)
  - [benbrandt/wasi-wheels](https://github.com/benbrandt/wasi-wheels/) (see also [this PR](https://github.com/benbrandt/wasi-wheels/pull/272) for an example of adding a new wheel)]

### Native Extensions

Packages with native C extensions (like `numpy`, `pandas`) require WASI-compiled wheels. Check if WASI builds are available for your specific package.

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
print(result.stdout)
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
print(result.stdout)  # "123"
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
