# MCP Integration

[MCP (Model Context Protocol)](https://modelcontextprotocol.io/) is a standard for AI assistants to communicate with tool servers. Eryx can discover and connect to MCP servers, exposing their tools as async callbacks inside the sandbox.

This means sandboxed Python code can call tools from any MCP server — filesystem access, GitHub, databases, web search, and more — all while maintaining the security of the sandbox.

## How It Works

MCP tools are bridged into the sandbox as async callbacks. The entire call path is handled in Rust, bypassing the Python GIL:

```text
Sandbox Python code
    → await mcp.server.tool(args)
    → WASM callback invoke
    → Rust DynamicCallback
    → rmcp client
    → MCP server (stdio)
```

From Python's perspective, MCP tools appear as dotted async functions under a namespace:

```python,no_test
# Inside the sandbox
result = await mcp.github.search_repos(query="eryx sandbox")
print(result["items"])

result = await mcp.filesystem.read_file(path="/tmp/data.txt")
print(result["text"])
```

## Config Discovery

When `--mcp` is passed (or `MCPManager` is used programmatically), Eryx searches for MCP server configurations from multiple IDEs and tools. Only `stdio`-type servers are supported.

### Supported Config Files

**Global (user-wide):**

| Tool | Path | Key |
|------|------|-----|
| Claude Code | `~/.claude.json` | `mcpServers` |
| Cursor | `~/.cursor/mcp.json` | `mcpServers` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | `mcpServers` |
| Zed | `~/.config/zed/settings.json` | `context_servers` |
| Gemini CLI | `~/.gemini/settings.json` | `mcpServers` |
| Codex | `~/.codex/config.toml` | `mcp_servers` |

**Project (cwd-relative):**

| Tool | Path | Key |
|------|------|-----|
| Claude Code | `.mcp.json` | `mcpServers` |
| Cursor | `.cursor/mcp.json` | `mcpServers` |
| VS Code | `.vscode/mcp.json` | `servers` |
| Zed | `.zed/settings.json` | `context_servers` |
| Gemini CLI | `.gemini/settings.json` | `mcpServers` |
| Codex | `.codex/config.toml` | `mcp_servers` |

Later sources override earlier ones, so project-level configs take precedence over global ones.

### Config Format

Most tools use the same JSON format. Here's a `.mcp.json` example:

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@anthropic/mcp-server-github"],
      "env": {
        "GITHUB_TOKEN": "${GITHUB_TOKEN}"
      }
    },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@anthropic/mcp-server-filesystem", "/tmp"]
    }
  }
}
```

Environment variables are expanded in `env` values using `$VAR`, `${VAR}`, or `${VAR:-default}` syntax.

Servers with `"disabled": true`, `"enabled": false`, or a non-stdio `"type"` are skipped.

## CLI Usage

Enable MCP with the `--mcp` flag:

```bash
# Auto-discover servers from all supported config files
python -m eryx --mcp -c 'tools = list_callbacks(); print([t["name"] for t in tools])'

# Use a specific config file
python -m eryx --mcp-config .mcp.json -c 'r = await mcp.github.search_repos(query="eryx"); print(r)'

# Multiple config files (later overrides earlier)
python -m eryx --mcp-config global.json --mcp-config project.json -c '...'
```

| Flag | Description |
|------|-------------|
| `--mcp` | Enable MCP server integration (auto-discovers from all supported configs) |
| `--mcp-config PATH` | Path to MCP config file (implies `--mcp`, can be repeated) |

## Python API

### Using `connect_servers` (High-Level)

The `eryx.mcp` module provides a high-level helper that discovers and connects to servers:

<!-- langtabs-start -->

```python,no_test
import eryx
from eryx.mcp import connect_servers

# Auto-discover and connect to all configured MCP servers
manager = connect_servers()

if manager is not None:
    # List discovered tools
    for tool in manager.list_tools():
        print(f"{tool['name']}: {tool['description']}")

    # Use with a sandbox
    sandbox = eryx.Sandbox(mcp=manager)
    result = sandbox.execute("""
r = await mcp.github.search_repos(query="python sandbox")
print(f"Found {len(r['items'])} repos")
""")
    print(result.stdout)

    # Clean up
    manager.close()
```

<!-- langtabs-end -->

### Using `MCPManager` (Low-Level)

For more control, create and configure the `MCPManager` directly:

<!-- langtabs-start -->

```python,no_test
import eryx

manager = eryx.MCPManager()

# Connect to specific servers
manager.connect(
    "my-server",           # server name
    "npx",                 # command
    ["-y", "my-mcp-pkg"],  # args
    {"API_KEY": "sk-..."},  # env
    30.0,                   # timeout (seconds)
)

print(f"Servers: {manager.server_names}")
print(f"Tools: {[t['name'] for t in manager.list_tools()]}")

# Use with Sandbox or Session
sandbox = eryx.Sandbox(mcp=manager)
session = eryx.Session(mcp=manager)

# Always close when done
manager.close()
```

<!-- langtabs-end -->

### Combining MCP with Python Callbacks

MCP tools and Python callbacks can be used together:

<!-- langtabs-start -->

```python,no_test
import eryx
from eryx.mcp import connect_servers

def my_transform(text: str):
    return {"result": text.upper()}

manager = connect_servers()

sandbox = eryx.Sandbox(
    mcp=manager,
    callbacks=[
        {"name": "transform", "fn": my_transform, "description": "Uppercases text"}
    ],
)

result = sandbox.execute("""
# MCP tool
data = await mcp.filesystem.read_file(path="/tmp/input.txt")

# Python callback
upper = await transform(text=data["text"])
print(upper["result"])
""")
```

<!-- langtabs-end -->

## Tool Naming

MCP tools are exposed in the sandbox with dotted names following the pattern `mcp.<server>.<tool>`:

```python,no_test
# Inside the sandbox
result = await mcp.github.search_repos(query="test")
#              ^^^  ^^^^^^  ^^^^^^^^^^^^
#              |    |       tool name
#              |    server name
#              mcp namespace
```

You can also use the generic `invoke()` function:

```python,no_test
result = await invoke("mcp.github.search_repos", query="test")
```

## Introspection

Discover available MCP tools at runtime:

```python,no_test
# Inside the sandbox
callbacks = list_callbacks()
mcp_tools = [c for c in callbacks if c["name"].startswith("mcp.")]
for tool in mcp_tools:
    print(f"{tool['name']}: {tool['description']}")
    print(f"  Parameters: {tool['schema']}")
```

## Best Practices

1. **Use auto-discovery for convenience** — `--mcp` finds servers from all your IDE configs automatically
2. **Use explicit configs for reproducibility** — `--mcp-config` ensures consistent behavior across machines
3. **Close the manager** — Always call `manager.close()` when done to terminate MCP server processes
4. **Handle missing tools gracefully** — Not all servers may be available in every environment
5. **Set appropriate timeouts** — The default 30s connection timeout can be adjusted via `connect_timeout`

## Next Steps

- [Callbacks](./callbacks.md) — Define custom host-side callbacks
- [CLI](./cli.md) — Full CLI reference
- [Sessions](./sessions.md) — Persistent state across executions
