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

from eryx._cli import add_sandbox_args, make_mcp_manager, make_net_config, make_resource_limits


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="eryx serve",
        description="Start an MCP server that exposes the eryx sandbox as a tool.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            examples:
              eryx serve                              basic sandbox server
              eryx serve --timeout 60000              60s execution timeout
              eryx serve --net                        with network access
              eryx serve --mcp                        with inner MCP tools
              eryx serve --mcp-config mcp.json        with explicit MCP config
        """),
    )

    add_sandbox_args(parser)

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
    mcp_manager = make_mcp_manager(args)

    # Build session kwargs
    session_kwargs: dict = {}
    limits = make_resource_limits(args)
    if limits is not None:
        session_kwargs["execution_timeout_ms"] = limits.execution_timeout_ms
    net = make_net_config(args)
    if net is not None:
        session_kwargs["network"] = net
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
