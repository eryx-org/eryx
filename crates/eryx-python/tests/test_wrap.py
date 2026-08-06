"""Integration tests for ``eryx wrap`` MCP meta-server.

Starts the wrap server wrapping a mock MCP server, connects as a real MCP
client over stdio, and exercises list_tools / call_tool / execute_python.

Requires the ``mcp`` package (``pip install 'pyeryx[serve]'``).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

MOCK_SERVER = str(Path(__file__).parent / "mock_mcp_server.py")
SERVER_NAME = "mock"


async def _run_with_session(
    fn,
    extra_args: list[str] | None = None,
    server_name: str = SERVER_NAME,
):
    """Start ``eryx wrap`` wrapping the mock server, run *fn(session)*, tear down."""
    args = ["-m", "eryx", "wrap", "--server-name", server_name]
    if extra_args:
        args.extend(extra_args)
    args.extend(["--", sys.executable, MOCK_SERVER])

    params = StdioServerParameters(
        command=sys.executable,
        args=args,
    )
    async with stdio_client(params) as (read_stream, write_stream):
        async with ClientSession(read_stream, write_stream) as session:
            await session.initialize()
            return await fn(session)


# -- Tool discovery ----------------------------------------------------------


@pytest.mark.asyncio
async def test_exposes_three_meta_tools():
    """Wrap server exposes exactly list_tools, call_tool, and execute_python."""

    async def body(session):
        result = await session.list_tools()
        names = sorted(t.name for t in result.tools)
        assert names == ["call_tool", "execute_python", "list_tools"]

    await _run_with_session(body)


# -- list_tools --------------------------------------------------------------


@pytest.mark.asyncio
async def test_list_tools_returns_wrapped_tools():
    """list_tools returns tools from the wrapped mock server."""

    async def body(session):
        result = await session.call_tool("list_tools", {})
        tools = json.loads(result.content[0].text)
        names = [t["name"] for t in tools]
        assert any("echo" in n for n in names)
        assert any("add" in n for n in names)

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_list_tools_include_schemas():
    """list_tools with include_schemas=true includes inputSchema."""

    async def body(session):
        result = await session.call_tool("list_tools", {"include_schemas": True})
        tools = json.loads(result.content[0].text)
        assert len(tools) > 0
        assert "inputSchema" in tools[0]

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_list_tools_filter_by_server():
    """list_tools with server filter returns only matching tools."""

    async def body(session):
        result = await session.call_tool("list_tools", {"server": SERVER_NAME})
        tools = json.loads(result.content[0].text)
        assert len(tools) >= 2  # echo + add
        for t in tools:
            assert f'mcp["{SERVER_NAME}"].' in t["name"]

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_list_tools_filter_unknown_server():
    """list_tools with unknown server filter returns empty list."""

    async def body(session):
        result = await session.call_tool("list_tools", {"server": "nonexistent"})
        tools = json.loads(result.content[0].text)
        assert tools == []

    await _run_with_session(body)


# -- call_tool ---------------------------------------------------------------


@pytest.mark.asyncio
async def test_call_tool_echo():
    """call_tool routes to the echo tool on the mock server."""

    async def body(session):
        result = await session.call_tool(
            "call_tool",
            {"name": f"{SERVER_NAME}.echo", "arguments": {"message": "hello wrap"}},
        )
        text = result.content[0].text
        assert "hello wrap" in text

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_call_tool_add():
    """call_tool routes to the add tool on the mock server."""

    async def body(session):
        result = await session.call_tool(
            "call_tool",
            {"name": f"{SERVER_NAME}.add", "arguments": {"a": 3, "b": 7}},
        )
        text = result.content[0].text
        parsed = json.loads(text)
        assert parsed["result"] == 10

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_call_tool_bracket_notation():
    """call_tool accepts bracket notation from list_tools output."""

    async def body(session):
        result = await session.call_tool(
            "call_tool",
            {
                "name": f'mcp["{SERVER_NAME}"].echo',
                "arguments": {"message": "bracket test"},
            },
        )
        text = result.content[0].text
        assert "bracket test" in text

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_call_tool_unknown_server():
    """call_tool with unknown server returns an error."""

    async def body(session):
        result = await session.call_tool(
            "call_tool",
            {"name": "nonexistent.echo", "arguments": {"message": "test"}},
        )
        text = result.content[0].text
        assert "unknown" in text.lower() or "error" in text.lower()

    await _run_with_session(body)


# -- execute_python ----------------------------------------------------------


@pytest.mark.asyncio
async def test_execute_python_basic():
    """execute_python runs Python code and returns output."""

    async def body(session):
        result = await session.call_tool(
            "execute_python", {"code": 'print("hello from wrap")'}
        )
        assert "hello from wrap" in result.content[0].text

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_execute_python_state_persistence():
    """Variables set in one execute_python call persist to the next."""

    async def body(session):
        await session.call_tool("execute_python", {"code": "x = 42"})
        result = await session.call_tool("execute_python", {"code": "print(x)"})
        assert "42" in result.content[0].text

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_execute_python_mcp_access():
    """execute_python can call wrapped MCP tools via await."""

    async def body(session):
        result = await session.call_tool(
            "execute_python",
            {"code": f'result = await mcp["{SERVER_NAME}"].echo(message="from python")\nprint(result)'},
        )
        assert "from python" in result.content[0].text

    await _run_with_session(body)


@pytest.mark.asyncio
async def test_execute_python_no_output():
    """execute_python with no print returns (no output)."""

    async def body(session):
        result = await session.call_tool("execute_python", {"code": "y = 1"})
        assert result.content[0].text == "(no output)"

    await _run_with_session(body)


# -- Custom server name ------------------------------------------------------


@pytest.mark.asyncio
async def test_custom_server_name():
    """--server-name sets the server name for inline commands."""

    async def body(session):
        result = await session.call_tool("list_tools", {})
        tools = json.loads(result.content[0].text)
        names = [t["name"] for t in tools]
        assert any('mcp["myserver"].' in n for n in names)

    await _run_with_session(body, server_name="myserver")
