"""MCP server that exposes the eryx sandbox as a ``run_python`` tool.

Start with::

    eryx serve                        # basic sandbox
    eryx serve --mcp                  # with inner MCP tools discovered from IDE configs
    eryx serve --mcp-config mcp.json  # with explicit inner MCP config

Or as a one-liner via uv::

    uvx --with 'pyeryx[serve]' eryx serve
"""

from __future__ import annotations

import argparse
import sys
import textwrap

import eryx


def _parse_volume(spec: str) -> tuple[str, str, bool]:
    """Parse a Docker-style volume spec: SRC:DST[:ro|:rw]."""
    parts = spec.split(":")
    if len(parts) == 2:
        return (parts[0], parts[1], False)
    if len(parts) == 3 and parts[2] in ("ro", "rw"):
        return (parts[0], parts[1], parts[2] == "ro")
    raise argparse.ArgumentTypeError(
        f"invalid volume format '{spec}', expected SRC:DST or SRC:DST:ro"
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="eryx serve",
        description="Start an MCP server that exposes the eryx sandbox as a tool.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            examples:
              eryx serve                              basic sandbox server
              eryx serve --timeout 60000              60s execution timeout
              eryx serve --mcp                        with inner MCP tools
              eryx serve --mcp-config mcp.json        with explicit MCP config
        """),
    )

    limits = parser.add_argument_group("resource limits")
    limits.add_argument(
        "--timeout",
        type=int,
        default=None,
        metavar="MS",
        help="execution timeout in milliseconds (default: 30000)",
    )

    mcp_group = parser.add_argument_group("MCP (inner tools)")
    mcp_group.add_argument(
        "--mcp",
        action="store_true",
        default=False,
        help="discover inner MCP servers from Claude, Cursor, VS Code, Zed, Windsurf, Codex, Gemini configs",
    )
    mcp_group.add_argument(
        "--mcp-config",
        action="append",
        default=[],
        metavar="PATH",
        help="path to MCP config file for inner tools (implies --mcp, can be repeated)",
    )

    fs = parser.add_argument_group("filesystem")
    fs.add_argument(
        "-v",
        "--volume",
        action="append",
        default=[],
        type=_parse_volume,
        metavar="SRC:DST[:ro]",
        help="mount host directory SRC at sandbox path DST (append :ro for read-only)",
    )

    return parser


def _build_tool_description(mcp_manager: object | None) -> str:
    """Build the run_python tool description, including available inner MCP tools."""
    desc = (
        "Execute Python code in a persistent sandboxed environment. "
        "State (variables, imports, functions) persists across calls. "
        "Use `print()` to produce output."
    )

    if mcp_manager is not None:
        tools = mcp_manager.list_tools()  # type: ignore[union-attr]
        if tools:
            desc += "\n\nAvailable tools inside the sandbox (call with `await`):"
            for t in tools:
                name = t["name"]
                tool_desc = t.get("description", "")
                if tool_desc:
                    if len(tool_desc) > 120:
                        tool_desc = tool_desc[:117] + "..."
                    desc += f"\n- `await {name}(...)`: {tool_desc}"
                else:
                    desc += f"\n- `await {name}(...)`"

    return desc


def serve(argv: list[str] | None = None) -> int:
    """Run the eryx MCP server over stdio."""
    try:
        from mcp.server.fastmcp import FastMCP
    except ImportError:
        print(
            "eryx serve requires the 'mcp' package.\n"
            "Install it with: pip install 'pyeryx[serve]'\n"
            "Or run with:     uvx --with 'pyeryx[serve]' eryx serve",
            file=sys.stderr,
        )
        return 1

    args = _build_parser().parse_args(argv)

    # Connect to inner MCP servers if requested
    mcp_manager = None
    if args.mcp or args.mcp_config:
        from eryx.mcp import connect_servers

        config_paths = args.mcp_config if args.mcp_config else None
        mcp_manager = connect_servers(config_paths=config_paths)

    # Build session
    session_kwargs: dict = {}
    if args.timeout is not None:
        session_kwargs["execution_timeout_ms"] = args.timeout
    if args.volume:
        session_kwargs["volumes"] = args.volume
    if mcp_manager is not None:
        session_kwargs["mcp"] = mcp_manager

    # Mutable buffers for capturing output per-execution
    stdout_chunks: list[str] = []
    stderr_chunks: list[str] = []
    session_kwargs["on_stdout"] = lambda chunk: stdout_chunks.append(chunk)
    session_kwargs["on_stderr"] = lambda chunk: stderr_chunks.append(chunk)

    session = eryx.Session(**session_kwargs)

    # Create the MCP server
    tool_description = _build_tool_description(mcp_manager)
    server = FastMCP("eryx")

    @server.tool(description=tool_description)
    def run_python(code: str, timeout_ms: int | None = None) -> str:
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
        if mcp_manager is not None:
            mcp_manager.close()

    return 0
