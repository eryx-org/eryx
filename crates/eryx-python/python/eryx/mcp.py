"""MCP server discovery and connection helpers.

Discovers MCP servers from configuration files across multiple IDEs and
tools, then connects to them using the Rust MCPManager.

Supported config sources (searched in order, later entries override earlier):

**Global (user-wide):**
- ``~/.claude.json`` — Claude Code (key: ``mcpServers``)
- ``~/.cursor/mcp.json`` — Cursor (key: ``mcpServers``)
- ``~/.codeium/windsurf/mcp_config.json`` — Windsurf (key: ``mcpServers``)
- ``~/.config/zed/settings.json`` — Zed on Linux (key: ``context_servers``)
- ``~/.gemini/settings.json`` — Gemini CLI (key: ``mcpServers``)
- ``~/.codex/config.toml`` — OpenAI Codex (key: ``mcp_servers``, TOML)

**Project (cwd-relative):**
- ``.mcp.json`` — Claude Code (key: ``mcpServers``)
- ``.cursor/mcp.json`` — Cursor (key: ``mcpServers``)
- ``.vscode/mcp.json`` — VS Code (key: ``servers``)
- ``.zed/settings.json`` — Zed (key: ``context_servers``)
- ``.gemini/settings.json`` — Gemini CLI (key: ``mcpServers``)
- ``.codex/config.toml`` — OpenAI Codex (key: ``mcp_servers``, TOML)

Example:
    from eryx.mcp import connect_servers

    manager = connect_servers()
    if manager is not None:
        tools = manager.list_tools()
        print(f"Connected to {len(manager.server_names)} servers, {len(tools)} tools")
"""

from __future__ import annotations

import json
import os
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional, Sequence

from eryx._eryx import MCPManager as _RustMCPManager


@dataclass(frozen=True)
class _ConfigSource:
    """A config file location with its MCP servers key and format."""

    path: Path
    key: str
    fmt: str = "json"  # "json" or "toml"


def _default_sources() -> list[_ConfigSource]:
    """Return the default config sources to search."""
    home = Path.home()
    cwd = Path.cwd()
    return [
        # Global (user-wide) configs
        _ConfigSource(home / ".claude.json", "mcpServers"),
        _ConfigSource(home / ".cursor" / "mcp.json", "mcpServers"),
        _ConfigSource(home / ".codeium" / "windsurf" / "mcp_config.json", "mcpServers"),
        _ConfigSource(home / ".config" / "zed" / "settings.json", "context_servers"),
        _ConfigSource(home / ".gemini" / "settings.json", "mcpServers"),
        _ConfigSource(home / ".codex" / "config.toml", "mcp_servers", "toml"),
        # Project (cwd-relative) configs — later overrides earlier
        _ConfigSource(cwd / ".mcp.json", "mcpServers"),
        _ConfigSource(cwd / ".cursor" / "mcp.json", "mcpServers"),
        _ConfigSource(cwd / ".vscode" / "mcp.json", "servers"),
        _ConfigSource(cwd / ".zed" / "settings.json", "context_servers"),
        _ConfigSource(cwd / ".gemini" / "settings.json", "mcpServers"),
        _ConfigSource(cwd / ".codex" / "config.toml", "mcp_servers", "toml"),
    ]


def _expand_env_vars(value: str) -> str:
    """Expand environment variables in a string.

    Supports:
    - ``$VAR`` and ``${VAR}`` — standard expansion
    - ``${VAR:-default}`` — default value if unset or empty
    """

    def _replace(m: re.Match[str]) -> str:
        name = m.group("name") or m.group("brace")
        if name:
            return os.environ.get(name, "")
        # ${VAR:-default} form
        name_with_default = m.group("defname")
        default = m.group("default") or ""
        return os.environ.get(name_with_default, "") or default

    pattern = (
        r"\$(?:"
        r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
        r"|"
        r"\{(?P<brace>[A-Za-z_][A-Za-z0-9_]*)\}"
        r"|"
        r"\{(?P<defname>[A-Za-z_][A-Za-z0-9_]*):-(?P<default>[^}]*)\}"
        r")"
    )
    return re.sub(pattern, _replace, value)


def _read_json(path: Path) -> dict[str, Any] | None:
    """Read a JSON file, returning None on any error."""
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None


def _read_toml(path: Path) -> dict[str, Any] | None:
    """Read a TOML file, returning None on any error."""
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError, UnicodeDecodeError):
        return None


def _extract_stdio_servers(
    data: dict[str, Any], key: str
) -> dict[str, dict[str, Any]]:
    """Extract stdio MCP server configs from a parsed config file.

    Args:
        data: Parsed config file contents.
        key: The key containing MCP server definitions (e.g. ``mcpServers``).

    Returns:
        Dict mapping server name to normalised config with ``command``,
        ``args``, and ``env`` keys.
    """
    servers: dict[str, dict[str, Any]] = {}
    mcp_servers = data.get(key, {})
    if not isinstance(mcp_servers, dict):
        return servers

    for name, config in mcp_servers.items():
        if not isinstance(config, dict):
            continue
        if config.get("disabled", False):
            continue
        if config.get("enabled") is False:
            continue

        server_type = config.get("type", "stdio")
        if server_type != "stdio":
            continue

        # Skip remote-only entries (no command, only url)
        if not config.get("command"):
            if config.get("url") or config.get("serverUrl") or config.get("httpUrl"):
                continue

        command = config.get("command")
        if not command:
            continue

        servers[name] = {
            "command": command,
            "args": config.get("args", []),
            "env": config.get("env", {}),
        }

    return servers


def discover_servers(
    config_paths: Sequence[str | Path] | None = None,
) -> dict[str, dict[str, Any]]:
    """Discover MCP server configurations from config files.

    When *config_paths* is ``None`` (the default), searches config files
    from multiple IDEs and tools.  See the module docstring for the full
    list.  Later sources override earlier ones, so project-level configs
    take precedence over global ones.

    Only servers with ``"type": "stdio"`` (or no ``type`` key, assumed
    stdio) are included.  Servers with ``"disabled": true`` or
    ``"enabled": false`` are skipped.

    Args:
        config_paths: Optional explicit config file paths to read instead
            of the default locations.  All are treated as JSON with key
            ``mcpServers``.

    Returns:
        Dict mapping server name → config dict with ``command``, ``args``,
        and ``env`` keys.
    """
    servers: dict[str, dict[str, Any]] = {}

    if config_paths is not None:
        # Explicit paths — all treated as JSON with "mcpServers" key
        sources = [_ConfigSource(Path(p), "mcpServers") for p in config_paths]
    else:
        sources = _default_sources()

    for source in sources:
        if source.fmt == "toml":
            data = _read_toml(source.path)
        else:
            data = _read_json(source.path)
        if data is None:
            continue

        found = _extract_stdio_servers(data, source.key)
        servers.update(found)

    return servers


def connect_servers(
    config_paths: Sequence[str | Path] | None = None,
    connect_timeout: float = 30.0,
) -> Optional[_RustMCPManager]:
    """Discover and connect to MCP servers.

    Args:
        config_paths: Optional explicit config file paths. Defaults to
            ``~/.claude.json`` and ``.mcp.json`` in cwd.
        connect_timeout: Timeout in seconds for each server connection.

    Returns:
        An ``MCPManager`` with all successfully connected servers, or ``None``
        if no servers were discovered or all connections failed.
    """
    servers = discover_servers(config_paths)
    if not servers:
        return None

    manager = _RustMCPManager()
    connected = 0

    for name, config in servers.items():
        env = {k: _expand_env_vars(str(v)) for k, v in config.get("env", {}).items()}
        try:
            tool_count = manager.connect(
                name,
                config["command"],
                config.get("args", []),
                env,
                connect_timeout,
            )
            connected += 1
            print(
                f"MCP: connected to '{name}' ({tool_count} tools)",
                file=sys.stderr,
            )
        except Exception as exc:
            print(
                f"MCP: failed to connect to '{name}': {exc}",
                file=sys.stderr,
            )

    if connected == 0:
        return None

    return manager
