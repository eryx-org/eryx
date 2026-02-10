# MCP Server

Eryx can run as an [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) server, exposing the sandbox as a `run_python` tool. This lets any MCP-compatible AI assistant execute Python code in a secure sandbox.

## Quick Start

```bash
# Run directly
uvx --with 'pyeryx[serve]' eryx serve

# Or if pyeryx is installed
eryx serve
```

This starts an MCP server over stdio with a single `run_python` tool. Any MCP client (Claude Desktop, Cursor, VS Code, etc.) can connect and execute Python code in the sandbox.

## Configuration

Add eryx to your MCP client config. For example, in Claude Desktop's config (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "eryx": {
      "command": "uvx",
      "args": ["--with", "pyeryx[serve]", "eryx", "serve"]
    }
  }
}
```

Or with sandbox options:

```json
{
  "mcpServers": {
    "eryx": {
      "command": "uvx",
      "args": [
        "--with", "pyeryx[serve]", "eryx", "serve",
        "--timeout", "60000",
        "--net",
        "--allow-host", "*.example.com"
      ]
    }
  }
}
```

## Flags

`eryx serve` accepts the same sandbox configuration flags as the main CLI:

| Flag | Description | Default |
|------|-------------|---------|
| `--timeout MS` | Execution timeout in milliseconds | 30000 |
| `--max-memory BYTES` | Maximum memory in bytes | 128MB |
| `--net` | Enable network access | off |
| `--allow-host PATTERN` | Allow access to matching hosts (implies `--net`) | |
| `-v SRC:DST[:ro]` | Mount host directory at sandbox path | |
| `--mcp` | Enable inner MCP tools (auto-discovers from IDE configs) | off |
| `--mcp-config PATH` | Path to inner MCP config file (implies `--mcp`) | |

## The `run_python` Tool

The server exposes a single tool:

| Parameter | Type | Description |
|-----------|------|-------------|
| `code` | `string` | Python code to execute (required) |
| `timeout_ms` | `integer` | Per-execution timeout override in milliseconds (optional) |

The tool uses a persistent `Session` under the hood, so variables, imports, and function definitions persist across calls. Output is captured from `print()` statements and returned as the tool result. Errors (execution errors, timeouts, resource limits) are returned as text rather than MCP errors, so the AI can see and react to them.

## Inner MCP Tools

A powerful pattern is running eryx as an MCP server with *inner* MCP tools. This gives the sandbox access to other MCP servers while still keeping execution sandboxed:

```bash
# Sandbox has access to tools from your IDE configs
eryx serve --mcp

# Sandbox has access to tools from a specific config
eryx serve --mcp-config .mcp.json
```

When inner MCP tools are enabled, the `run_python` tool description is automatically updated to list the available tools, so the AI knows what's callable inside the sandbox:

```text
Execute Python code in a persistent sandboxed environment. ...

Available tools inside the sandbox (call with `await`):
- `await mcp["github"].search_repos(...)`: Search GitHub repositories
- `await mcp["filesystem"].read_file(...)`: Read a file from the filesystem
```

This is useful when you want an AI to have both code execution *and* tool access, but want all of it to run through a single sandboxed environment.

## Examples

```bash
# Basic sandbox server
eryx serve

# With a 60-second timeout
eryx serve --timeout 60000

# With network access
eryx serve --net

# With network access restricted to specific hosts
eryx serve --net --allow-host 'api.example.com' --allow-host '*.internal.dev'

# With a mounted directory
eryx serve -v /path/to/data:/data:ro

# With inner MCP tools from IDE configs
eryx serve --mcp

# With inner MCP tools from a specific config
eryx serve --mcp-config path/to/mcp.json

# Combining options
eryx serve --timeout 60000 --net --mcp -v ./workspace:/workspace
```

## Next Steps

- [MCP Integration](./mcp.md) -- Using MCP tools *inside* the sandbox
- [CLI](./cli.md) -- Full CLI reference
- [Sessions](./sessions.md) -- How persistent state works
- [Networking](./networking.md) -- Network access configuration
