# Command-Line Interface

Eryx includes a CLI for running Python code in a sandbox directly from the terminal. It supports an interactive REPL, script execution, inline code, and stdin.

## Installation

The CLI is included with the Python package:

```bash
pip install pyeryx
```

Or run without installing using `uvx`:

```bash
uvx --with pyeryx eryx
```

## Usage Modes

### Interactive REPL

Start an interactive session with persistent state:

```bash
python -m eryx
```

```text
Eryx 0.3.1 (sandbox REPL)
Type "exit()" or Ctrl-D to quit.
>>> x = 42
>>> print(x * 2)
84
>>> exit()
```

The REPL uses a `Session` under the hood, so variables, functions, and classes persist between inputs. Multi-line blocks (functions, classes, loops) are detected automatically.

### Execute a Script File

```bash
python -m eryx script.py
```

### Execute Inline Code

```bash
python -m eryx -c 'print("Hello from the sandbox!")'
```

### Read from Stdin

```bash
echo 'print(1 + 1)' | python -m eryx -
```

Or pipe without the `-` flag:

```bash
echo 'import sys; print(sys.version)' | python -m eryx
```

## Flags

### Resource Limits

| Flag                 | Description                       | Default |
| -------------------- | --------------------------------- | ------- |
| `--timeout MS`       | Execution timeout in milliseconds | 30000   |
| `--max-memory BYTES` | Maximum memory in bytes           | 128MB   |

```bash
python -m eryx --timeout 5000 -c 'print("quick")'
python -m eryx --max-memory 67108864 script.py   # 64MB limit
```

### Networking

| Flag                   | Description                                              |
| ---------------------- | -------------------------------------------------------- |
| `--net`                | Enable network access                                    |
| `--allow-host PATTERN` | Allow access to hosts matching pattern (implies `--net`) |

```bash
python -m eryx --net -c 'import urllib.request; ...'
python -m eryx --allow-host '*.example.com' -c '...'
```

### Filesystem

| Flag              | Description                                  |
| ----------------- | -------------------------------------------- |
| `-v SRC:DST[:ro]` | Mount host directory SRC at sandbox path DST |

```bash
# Mount current directory read-write
python -m eryx -v $(pwd):/mnt/data script.py

# Mount read-only
python -m eryx -v /path/to/files:/input:ro -c 'print(open("/input/data.txt").read())'
```

### MCP (Model Context Protocol)

| Flag               | Description                                                            |
| ------------------ | ---------------------------------------------------------------------- |
| `--mcp`            | Enable MCP server integration (auto-discovers from IDE configs)        |
| `--mcp-config PATH`| Path to MCP config file (implies `--mcp`, can be repeated)            |

```bash
# Auto-discover MCP servers from Claude, Cursor, VS Code, Zed, Windsurf, Codex, Gemini configs
python -m eryx --mcp -c 'r = await mcp.github.search_repos(query="test"); print(r)'

# Use a specific config file
python -m eryx --mcp-config .mcp.json -c '...'
```

See [MCP Integration](./mcp.md) for details on config discovery and supported IDEs.

### Other

| Flag           | Description            |
| -------------- | ---------------------- |
| `--version`    | Print version and exit |
| `-h`, `--help` | Show help message      |

## Examples

```bash
# Quick one-liner
uvx --with pyeryx eryx -c 'import sys; print(f"Python {sys.version}")'

# Run a script with timeout
uvx --with pyeryx eryx --timeout 5000 script.py

# Network-enabled execution
uvx --with pyeryx eryx --net --allow-host 'httpbin.org' -c '
import urllib.request
resp = urllib.request.urlopen("https://httpbin.org/get")
print(resp.read().decode())
'

# Mount a local directory (read-only)
uvx --with pyeryx eryx -v ./data:/data:ro -c '
import os
print(os.listdir("/data"))
'
```

## Next Steps

- [Sandboxes](./sandboxes.md) — Sandbox configuration
- [MCP Integration](./mcp.md) — Connect to MCP tool servers
- [Resource Limits](./resource-limits.md) — Controlling execution constraints
- [VFS and File Persistence](./vfs.md) — Virtual filesystem and volume mounts
