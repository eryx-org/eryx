"""MCP server discovery and connection helpers.

Discovers MCP servers from configuration files (`.mcp.json` in cwd,
`~/.claude.json`) and connects to them using the Rust MCPManager.

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
from pathlib import Path
from typing import Any, Optional, Sequence

from eryx._eryx import MCPManager as _RustMCPManager


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


def discover_servers(
    config_paths: Sequence[str | Path] | None = None,
) -> dict[str, dict[str, Any]]:
    """Discover MCP server configurations from config files.

    Searches (in order, later entries override earlier):
    1. ``~/.claude.json`` — global Claude config (``mcpServers`` key)
    2. ``.mcp.json`` in the current working directory

    Only servers with ``"type": "stdio"`` or no ``type`` key (assumed stdio)
    are included. Servers with ``"disabled": true`` are skipped.

    Args:
        config_paths: Optional explicit config file paths to read instead
            of the default locations.

    Returns:
        Dict mapping server name → config dict with ``command``, ``args``,
        and ``env`` keys.
    """
    servers: dict[str, dict[str, Any]] = {}

    if config_paths is not None:
        paths = [Path(p) for p in config_paths]
    else:
        paths = [
            Path.home() / ".claude.json",
            Path.cwd() / ".mcp.json",
        ]

    for path in paths:
        data = _read_json(path)
        if data is None:
            continue

        mcp_servers = data.get("mcpServers", {})
        if not isinstance(mcp_servers, dict):
            continue

        for name, config in mcp_servers.items():
            if not isinstance(config, dict):
                continue
            if config.get("disabled", False):
                continue

            server_type = config.get("type", "stdio")
            if server_type != "stdio":
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
