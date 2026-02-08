"""MCP (Model Context Protocol) client for Eryx.

Discovers MCP servers from standard config files (Claude Code, Claude Desktop)
and exposes their tools as Eryx callbacks.

Config files searched (in order):
  1. Custom path via --mcp-config
  2. .mcp.json in the current working directory (project scope)
  3. ~/.claude.json (user/local scope)

Usage:
    from eryx.mcp import MCPManager

    manager = MCPManager()
    manager.discover()       # find servers from config files
    manager.connect_all()    # spawn processes and initialize
    callbacks = manager.as_callbacks()  # get eryx callback dicts
    manager.close()          # terminate server processes
"""

from __future__ import annotations

import json
import logging
import os
import re
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

logger = logging.getLogger("eryx.mcp")

# MCP protocol version we support
MCP_PROTOCOL_VERSION = "2024-11-05"


class MCPError(Exception):
    """Error communicating with an MCP server."""


class StdioMCPClient:
    """Client for a single MCP server over stdio transport.

    Spawns the server process and communicates via JSON-RPC 2.0
    messages over stdin/stdout.
    """

    def __init__(
        self,
        name: str,
        command: str,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> None:
        self.name = name
        self.command = command
        self.args = args or []
        self.env = env or {}
        self._process: subprocess.Popen[bytes] | None = None
        self._next_id = 1
        self._lock = threading.Lock()
        self._tools: list[dict[str, Any]] = []
        self._server_info: dict[str, Any] = {}

    @property
    def tools(self) -> list[dict[str, Any]]:
        """Tools discovered from this server."""
        return self._tools

    @property
    def server_info(self) -> dict[str, Any]:
        """Server info from the initialize response."""
        return self._server_info

    def connect(self, timeout: float = 30.0) -> None:
        """Spawn the server process and perform the MCP initialize handshake."""
        # Build environment: inherit current env + merge server-specific vars
        proc_env = os.environ.copy()
        for key, value in self.env.items():
            proc_env[key] = _expand_env_vars(value)

        cmd = [self.command] + self.args
        logger.debug("Starting MCP server %r: %s", self.name, cmd)

        try:
            self._process = subprocess.Popen(
                cmd,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=proc_env,
            )
        except FileNotFoundError:
            raise MCPError(
                f"MCP server {self.name!r}: command not found: {self.command!r}"
            )
        except OSError as exc:
            raise MCPError(
                f"MCP server {self.name!r}: failed to start: {exc}"
            )

        # Send initialize request
        init_result = self._request(
            "initialize",
            {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "eryx", "version": "0.1.0"},
            },
            timeout=timeout,
        )

        self._server_info = init_result or {}

        # Send initialized notification (no response expected)
        self._notify("notifications/initialized", {})

        # Discover tools
        tools_result = self._request("tools/list", {}, timeout=timeout)
        self._tools = (tools_result or {}).get("tools", [])
        logger.info(
            "MCP server %r: %d tools available", self.name, len(self._tools)
        )

    def call_tool(
        self, tool_name: str, arguments: dict[str, Any], timeout: float = 30.0
    ) -> Any:
        """Call a tool on this MCP server.

        Returns the tool result content.
        """
        result = self._request(
            "tools/call",
            {"name": tool_name, "arguments": arguments},
            timeout=timeout,
        )
        if result is None:
            return None

        # MCP tools return content as a list of content blocks
        content = result.get("content", [])
        is_error = result.get("isError", False)

        # Extract text from content blocks
        texts = []
        for block in content:
            if block.get("type") == "text":
                texts.append(block.get("text", ""))

        text_result = "\n".join(texts)

        if is_error:
            raise MCPError(f"MCP tool {tool_name} returned error: {text_result}")

        # Try to parse as JSON for structured results
        if len(texts) == 1:
            try:
                return json.loads(texts[0])
            except (json.JSONDecodeError, TypeError):
                pass

        return text_result if texts else result

    def close(self) -> None:
        """Terminate the server process."""
        if self._process is not None:
            try:
                if self._process.stdin:
                    self._process.stdin.close()
                self._process.terminate()
                try:
                    self._process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self._process.kill()
                    self._process.wait(timeout=2)
            except OSError:
                pass
            finally:
                self._process = None

    def _request(
        self, method: str, params: dict[str, Any], timeout: float = 30.0
    ) -> dict[str, Any] | None:
        """Send a JSON-RPC request and wait for the response."""
        with self._lock:
            msg_id = self._next_id
            self._next_id += 1

        request = {
            "jsonrpc": "2.0",
            "id": msg_id,
            "method": method,
            "params": params,
        }

        self._send(request)
        response = self._recv(timeout=timeout)

        if response is None:
            raise MCPError(
                f"MCP server {self.name!r}: no response to {method}"
            )

        if "error" in response:
            err = response["error"]
            code = err.get("code", "?")
            message = err.get("message", "unknown error")
            raise MCPError(
                f"MCP server {self.name!r}: {method} failed ({code}): {message}"
            )

        return response.get("result")

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        """Send a JSON-RPC notification (no response expected)."""
        notification = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }
        self._send(notification)

    def _send(self, message: dict[str, Any]) -> None:
        """Send a JSON-RPC message to the server's stdin."""
        if self._process is None or self._process.stdin is None:
            raise MCPError(f"MCP server {self.name!r}: not connected")

        data = json.dumps(message).encode("utf-8")
        # MCP uses newline-delimited JSON
        self._process.stdin.write(data + b"\n")
        self._process.stdin.flush()

    def _recv(self, timeout: float = 30.0) -> dict[str, Any] | None:
        """Read a JSON-RPC response from the server's stdout.

        Skips notification messages (no 'id' field) and reads until we get
        a response with an 'id' field.
        """
        if self._process is None or self._process.stdout is None:
            raise MCPError(f"MCP server {self.name!r}: not connected")

        import select

        while True:
            # Wait for data with timeout
            ready, _, _ = select.select(
                [self._process.stdout], [], [], timeout
            )
            if not ready:
                raise MCPError(
                    f"MCP server {self.name!r}: timeout waiting for response"
                )

            line = self._process.stdout.readline()
            if not line:
                # Process died
                stderr = ""
                if self._process.stderr:
                    stderr = self._process.stderr.read().decode(
                        "utf-8", errors="replace"
                    )
                raise MCPError(
                    f"MCP server {self.name!r}: process exited unexpectedly"
                    + (f": {stderr}" if stderr else "")
                )

            line = line.strip()
            if not line:
                continue

            try:
                msg = json.loads(line)
            except json.JSONDecodeError as exc:
                logger.warning(
                    "MCP server %r: invalid JSON: %s", self.name, exc
                )
                continue

            # Skip notifications (messages without 'id')
            if "id" not in msg:
                logger.debug(
                    "MCP server %r: notification: %s",
                    self.name,
                    msg.get("method", "?"),
                )
                continue

            return msg

    def __enter__(self) -> StdioMCPClient:
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    def __del__(self) -> None:
        self.close()


def _expand_env_vars(value: str) -> str:
    """Expand ${VAR} and ${VAR:-default} patterns in a string."""

    def _replacer(match: re.Match[str]) -> str:
        var_name = match.group(1)
        default = match.group(3)  # group 3 is after :-
        env_val = os.environ.get(var_name)
        if env_val is not None:
            return env_val
        if default is not None:
            return default
        return match.group(0)  # leave as-is if no default and not set

    return re.sub(r"\$\{(\w+)(:-([^}]*))?\}", _replacer, value)


def discover_servers(
    config_paths: list[Path] | None = None,
) -> dict[str, dict[str, Any]]:
    """Discover MCP server configurations from config files.

    Searches (in order of precedence):
      1. Explicit config_paths
      2. .mcp.json in cwd
      3. ~/.claude.json

    Returns a dict of server_name -> server_config.
    """
    servers: dict[str, dict[str, Any]] = {}

    paths_to_check: list[Path] = []

    if config_paths:
        paths_to_check.extend(config_paths)
    else:
        # Project scope: .mcp.json in cwd
        cwd_config = Path.cwd() / ".mcp.json"
        if cwd_config.is_file():
            paths_to_check.append(cwd_config)

        # User scope: ~/.claude.json
        home_config = Path.home() / ".claude.json"
        if home_config.is_file():
            paths_to_check.append(home_config)

    for path in paths_to_check:
        try:
            found = _parse_config_file(path)
            # First-seen wins (earlier paths have higher precedence)
            for name, config in found.items():
                if name not in servers:
                    servers[name] = config
        except Exception as exc:
            logger.warning("Failed to parse MCP config %s: %s", path, exc)

    return servers


def _parse_config_file(path: Path) -> dict[str, dict[str, Any]]:
    """Parse a single config file for MCP server definitions.

    Supports two formats:
      1. .mcp.json format: {"mcpServers": {...}}
      2. ~/.claude.json format: {"mcpServers": {...}}
    """
    with open(path) as f:
        data = json.load(f)

    if not isinstance(data, dict):
        return {}

    # Both formats use "mcpServers" key
    mcp_servers = data.get("mcpServers", {})
    if not isinstance(mcp_servers, dict):
        return {}

    result: dict[str, dict[str, Any]] = {}
    for name, config in mcp_servers.items():
        if not isinstance(config, dict):
            continue

        # Determine transport type
        server_type = config.get("type", "stdio")

        if server_type == "stdio":
            # stdio servers need a command
            command = config.get("command")
            if not command:
                logger.warning(
                    "MCP server %r in %s: missing 'command', skipping",
                    name,
                    path,
                )
                continue
            result[name] = {
                "type": "stdio",
                "command": command,
                "args": config.get("args", []),
                "env": config.get("env", {}),
            }
        elif server_type in ("http", "sse"):
            # Remote servers - skip for now (PoC focuses on stdio)
            logger.info(
                "MCP server %r in %s: %s transport not yet supported, skipping",
                name,
                path,
                server_type,
            )
        else:
            logger.warning(
                "MCP server %r in %s: unknown type %r, skipping",
                name,
                path,
                server_type,
            )

    return result


class MCPManager:
    """Manages multiple MCP server connections and exposes their tools as callbacks.

    Usage:
        manager = MCPManager()
        manager.discover()
        manager.connect_all()
        callbacks = manager.as_callbacks()
        # ... use callbacks with eryx.Sandbox(callbacks=callbacks)
        manager.close()
    """

    def __init__(
        self,
        config_paths: list[Path] | None = None,
        connect_timeout: float = 30.0,
        call_timeout: float = 30.0,
    ) -> None:
        self._config_paths = config_paths
        self._connect_timeout = connect_timeout
        self._call_timeout = call_timeout
        self._server_configs: dict[str, dict[str, Any]] = {}
        self._clients: dict[str, StdioMCPClient] = {}

    @property
    def server_names(self) -> list[str]:
        """Names of discovered servers."""
        return list(self._server_configs.keys())

    @property
    def connected_servers(self) -> list[str]:
        """Names of connected servers."""
        return list(self._clients.keys())

    def discover(self) -> dict[str, dict[str, Any]]:
        """Discover MCP servers from config files.

        Returns the discovered server configs.
        """
        self._server_configs = discover_servers(self._config_paths)
        logger.info("Discovered %d MCP servers", len(self._server_configs))
        return self._server_configs

    def connect_all(self) -> None:
        """Connect to all discovered stdio MCP servers.

        Servers that fail to connect are logged and skipped.
        """
        for name, config in self._server_configs.items():
            if config.get("type") != "stdio":
                continue
            try:
                self._connect_server(name, config)
            except MCPError as exc:
                logger.error("Failed to connect to MCP server %r: %s", name, exc)
                print(
                    f"eryx: warning: MCP server {name!r} failed to connect: {exc}",
                    file=sys.stderr,
                )

    def connect_server(self, name: str) -> None:
        """Connect to a specific discovered server by name."""
        if name not in self._server_configs:
            raise MCPError(f"Unknown MCP server: {name!r}")
        config = self._server_configs[name]
        self._connect_server(name, config)

    def _connect_server(self, name: str, config: dict[str, Any]) -> None:
        """Internal: connect to a single server."""
        client = StdioMCPClient(
            name=name,
            command=config["command"],
            args=config.get("args", []),
            env=config.get("env", {}),
        )
        client.connect(timeout=self._connect_timeout)
        self._clients[name] = client

    def all_tools(self) -> list[dict[str, Any]]:
        """Get all tools from all connected servers.

        Each tool dict includes a '_server' key with the server name.
        """
        tools = []
        for server_name, client in self._clients.items():
            for tool in client.tools:
                tools.append({**tool, "_server": server_name})
        return tools

    def as_callbacks(self) -> list[dict[str, Any]]:
        """Convert all MCP tools to eryx callback dicts.

        Each tool becomes a callback named "mcp.<server>.<tool>".
        """
        callbacks = []
        for server_name, client in self._clients.items():
            for tool in client.tools:
                tool_name = tool.get("name", "unknown")
                cb_name = f"mcp.{server_name}.{tool_name}"
                description = tool.get("description", "")
                input_schema = tool.get("inputSchema", {})

                # Create a closure that captures the right client and tool name
                def make_handler(
                    _client: StdioMCPClient,
                    _tool_name: str,
                    _timeout: float,
                ) -> Any:
                    def handler(**kwargs: Any) -> Any:
                        return _client.call_tool(
                            _tool_name, kwargs, timeout=_timeout
                        )

                    return handler

                cb: dict[str, Any] = {
                    "name": cb_name,
                    "fn": make_handler(client, tool_name, self._call_timeout),
                    "description": f"[MCP: {server_name}] {description}",
                }
                if input_schema:
                    cb["schema"] = input_schema

                callbacks.append(cb)

        return callbacks

    def close(self) -> None:
        """Close all connected MCP servers."""
        for name, client in self._clients.items():
            try:
                client.close()
            except Exception as exc:
                logger.warning("Error closing MCP server %r: %s", name, exc)
        self._clients.clear()

    def __enter__(self) -> MCPManager:
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    def __del__(self) -> None:
        self.close()
