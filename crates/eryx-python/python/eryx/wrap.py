"""MCP meta-server that wraps other MCP servers behind three tools.

Instead of exposing every tool from every server directly, ``eryx wrap``
presents a simplified interface: ``list_tools``, ``call_tool``, and
``execute_python``.  An LLM client sees 3 tools instead of N, and can
orchestrate across multiple servers via Python code in a single call.

Start with::

    eryx wrap -- npx @anthropic/mcp-filesystem       # single inline server
    eryx wrap --config servers.json                   # multi-server from config
    eryx wrap --mcp                                   # discover from IDE configs
    eryx wrap --mcp --timeout 60000 --net             # with sandbox options
    eryx wrap --server-name fs -- npx ...             # custom name for inline
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import textwrap

import eryx

from eryx._cli import add_sandbox_args, make_net_config, make_resource_limits
from eryx._eryx import MCPManager as _RustMCPManager
from eryx.mcp import _expand_env_vars, connect_servers, discover_servers


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="eryx wrap",
        description="Wrap MCP servers behind a meta-tool interface.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            examples:
              eryx wrap -- npx @anthropic/mcp-filesystem .
              eryx wrap --config servers.json
              eryx wrap --mcp
              eryx wrap --mcp --timeout 60000 --net
              eryx wrap --server-name fs -- npx -y @anthropic/mcp-filesystem .
        """),
    )

    parser.add_argument(
        "--config",
        action="append",
        default=[],
        metavar="PATH",
        help="path to MCP server config file (JSON with mcpServers key, can be repeated)",
    )
    parser.add_argument(
        "--server-name",
        default=None,
        metavar="NAME",
        help="custom name for the inline server specified after --",
    )

    add_sandbox_args(parser)

    return parser


def _derive_name(cmd: list[str]) -> str:
    """Derive a short server name from an inline command.

    Looks for the last argument that looks like a package name
    (e.g. ``@anthropic/mcp-filesystem`` → ``filesystem``).
    Falls back to the command basename.
    """
    # Walk args in reverse looking for a package-like token
    for token in reversed(cmd):
        # npm scoped package: @scope/mcp-server-foo → foo
        m = re.match(r"@[\w-]+/(?:mcp-(?:server-)?)?(.+)", token)
        if m:
            return m.group(1)
        # Plain package: mcp-server-foo → foo
        m = re.match(r"mcp-(?:server-)?(.+)", token)
        if m:
            return m.group(1)

    # Fallback to command basename
    return cmd[0].rsplit("/", 1)[-1] if cmd else "server"


def _split_argv(raw_args: list[str]) -> tuple[list[str], list[str]]:
    """Split raw argv at ``--`` into (wrap_args, inline_cmd).

    Returns (wrap_args, []) if no ``--`` separator is found.
    """
    try:
        idx = raw_args.index("--")
        return raw_args[:idx], raw_args[idx + 1 :]
    except ValueError:
        return raw_args, []


def _connect_servers(
    args: argparse.Namespace,
    inline_cmd: list[str],
) -> _RustMCPManager:
    """Build an MCPManager from all server sources (inline + config + discovery).

    Raises SystemExit if no servers could be connected.
    """
    manager = _RustMCPManager()
    connected = 0

    # 1. Inline command from -- separator
    if inline_cmd:
        name = args.server_name or _derive_name(inline_cmd)
        command = inline_cmd[0]
        cmd_args = inline_cmd[1:]
        try:
            tool_count = manager.connect(name, command, cmd_args)
            connected += 1
            print(f"MCP: connected to '{name}' ({tool_count} tools)", file=sys.stderr)
        except Exception as exc:
            print(f"MCP: failed to connect to '{name}': {exc}", file=sys.stderr)

    # 2. Config files (--config)
    if args.config:
        servers = discover_servers(config_paths=args.config)
        for name, config in servers.items():
            env = {k: _expand_env_vars(str(v)) for k, v in config.get("env", {}).items()}
            try:
                tool_count = manager.connect(
                    name, config["command"], config.get("args", []), env
                )
                connected += 1
                print(
                    f"MCP: connected to '{name}' ({tool_count} tools)",
                    file=sys.stderr,
                )
            except Exception as exc:
                print(f"MCP: failed to connect to '{name}': {exc}", file=sys.stderr)

    # 3. IDE discovery (--mcp / --mcp-config)
    if args.mcp or args.mcp_config:
        config_paths = args.mcp_config if args.mcp_config else None
        servers = discover_servers(config_paths=config_paths)
        for name, config in servers.items():
            # Skip if already connected (from inline or --config)
            if name in manager.server_names:
                continue
            env = {k: _expand_env_vars(str(v)) for k, v in config.get("env", {}).items()}
            try:
                tool_count = manager.connect(
                    name, config["command"], config.get("args", []), env
                )
                connected += 1
                print(
                    f"MCP: connected to '{name}' ({tool_count} tools)",
                    file=sys.stderr,
                )
            except Exception as exc:
                print(f"MCP: failed to connect to '{name}': {exc}", file=sys.stderr)

    if connected == 0:
        print(
            "eryx wrap: no MCP servers connected.\n"
            "Specify servers with --, --config, or --mcp.",
            file=sys.stderr,
        )
        raise SystemExit(1)

    return manager


def wrap(argv: list[str] | None = None) -> int:
    """Run the eryx wrap meta-server over stdio."""
    try:
        from mcp.server.fastmcp import FastMCP
    except ImportError:
        print(
            "eryx wrap requires the 'mcp' package.\n"
            "Install it with: pip install 'pyeryx[serve]'\n"
            "Or run with:     uvx --with 'pyeryx[serve]' eryx wrap",
            file=sys.stderr,
        )
        return 1

    raw_args = argv if argv is not None else sys.argv[1:]
    wrap_args, inline_cmd = _split_argv(raw_args)
    args = _build_parser().parse_args(wrap_args)

    # Connect to all wrapped MCP servers
    try:
        mcp_manager = _connect_servers(args, inline_cmd)
    except SystemExit:
        return 1

    # Build session kwargs for the sandbox
    session_kwargs: dict = {}
    limits = make_resource_limits(args)
    if limits is not None:
        session_kwargs["execution_timeout_ms"] = limits.execution_timeout_ms
    net = make_net_config(args)
    if net is not None:
        session_kwargs["network"] = net
    if args.volume:
        session_kwargs["volumes"] = args.volume
    session_kwargs["mcp"] = mcp_manager

    # Mutable buffers for capturing output per-execution
    stdout_chunks: list[str] = []
    stderr_chunks: list[str] = []
    session_kwargs["on_stdout"] = lambda chunk: stdout_chunks.append(chunk)
    session_kwargs["on_stderr"] = lambda chunk: stderr_chunks.append(chunk)

    session = eryx.Session(**session_kwargs)

    # Build tool descriptions
    all_tools = mcp_manager.list_tools()
    tool_summary = ", ".join(t["name"].split(".")[-1] for t in all_tools)

    server = FastMCP("eryx-wrap")

    @server.tool(
        description=(
            "List tools available from wrapped MCP servers. "
            "Returns tool names and descriptions by default; "
            "set include_schemas=true for full input schemas."
        )
    )
    def list_tools(
        server: str | None = None,
        include_schemas: bool = False,
    ) -> str:
        """List available tools from wrapped MCP servers."""
        tools = mcp_manager.list_tools()

        if server is not None:
            tools = [t for t in tools if t["name"].startswith(f'mcp["{server}"].')]

        result = []
        for t in tools:
            entry: dict = {"name": t["name"], "description": t["description"]}
            if include_schemas:
                entry["inputSchema"] = t["schema"]
            result.append(entry)

        return json.dumps(result, indent=2)

    @server.tool(
        description=(
            "Call a tool on a wrapped MCP server. "
            "Use server.tool or mcp[\"server\"].tool notation for the name. "
            f"Available tools: {tool_summary}"
        )
    )
    def call_tool(name: str, arguments: dict | None = None) -> str:
        """Invoke a tool on a wrapped MCP server."""
        result = mcp_manager.call_tool(name, arguments)
        if isinstance(result, str):
            return result
        return json.dumps(result, indent=2)

    @server.tool(
        description=(
            "Execute Python code in a persistent sandboxed environment. "
            "State (variables, imports, functions) persists across calls. "
            "All wrapped MCP tools are available via await, e.g.:\n"
            '  data = await mcp["server"].tool(arg="value")\n'
            "Use print() to produce output."
        )
    )
    def execute_python(code: str, timeout_ms: int | None = None) -> str:
        """Execute Python code in the eryx sandbox."""
        old_timeout = session.execution_timeout_ms
        if timeout_ms is not None:
            session.execution_timeout_ms = timeout_ms

        stdout_chunks.clear()
        stderr_chunks.clear()

        try:
            session.execute(code)
            stdout = "".join(stdout_chunks)
            stderr = "".join(stderr_chunks)
            parts = []
            if stdout:
                parts.append(stdout)
            if stderr:
                parts.append(f"[stderr]\n{stderr}")
            return "\n".join(parts) if parts else "(no output)"
        except (
            eryx.ExecutionError,
            eryx.TimeoutError,
            eryx.ResourceLimitError,
        ) as exc:
            stdout = "".join(stdout_chunks)
            stderr = "".join(stderr_chunks)
            parts = []
            if stdout:
                parts.append(stdout)
            if stderr:
                parts.append(f"[stderr]\n{stderr}")
            parts.append(str(exc))
            return "\n".join(parts)
        finally:
            if timeout_ms is not None:
                session.execution_timeout_ms = old_timeout

    try:
        server.run(transport="stdio")
    finally:
        mcp_manager.close()

    return 0
