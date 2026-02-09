"""Integration tests for ``eryx serve`` MCP server.

Starts the server as a subprocess and connects as a real MCP client over stdio.
Requires the ``mcp`` package (``pip install 'pyeryx[serve]'``).
"""

from __future__ import annotations

import sys

import pytest
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


async def _run_with_session(fn):
    """Start ``eryx serve``, connect a client, run *fn(session)*, then tear down.

    Uses anyio directly to avoid pytest-asyncio fixture teardown issues with
    anyio's cancel-scope task affinity checks.
    """
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "eryx", "serve"],
    )
    async with stdio_client(params) as (read_stream, write_stream):
        async with ClientSession(read_stream, write_stream) as session:
            await session.initialize()
            return await fn(session)


# -- Tool discovery ----------------------------------------------------------


@pytest.mark.asyncio
async def test_list_tools():
    """Server exposes a ``run_python`` tool with a ``code`` parameter."""

    async def body(session):
        result = await session.list_tools()
        names = [t.name for t in result.tools]
        assert "run_python" in names

        tool = next(t for t in result.tools if t.name == "run_python")
        schema = tool.inputSchema
        assert "code" in schema["properties"]

    await _run_with_session(body)


# -- Basic execution ---------------------------------------------------------


@pytest.mark.asyncio
async def test_basic_execution():
    """``print("hello")`` produces ``hello\\n``."""

    async def body(session):
        result = await session.call_tool("run_python", {"code": 'print("hello")'})
        assert result.content[0].text == "hello\n"

    await _run_with_session(body)


# -- State persistence -------------------------------------------------------


@pytest.mark.asyncio
async def test_state_persistence():
    """Variables set in one call are visible in subsequent calls."""

    async def body(session):
        await session.call_tool("run_python", {"code": "x = 42"})
        result = await session.call_tool("run_python", {"code": "print(x)"})
        assert result.content[0].text == "42\n"

    await _run_with_session(body)


# -- Error handling ----------------------------------------------------------


@pytest.mark.asyncio
async def test_execution_error():
    """``1/0`` returns a result containing ``ZeroDivisionError``."""

    async def body(session):
        result = await session.call_tool("run_python", {"code": "1/0"})
        assert "ZeroDivisionError" in result.content[0].text

    await _run_with_session(body)


# -- No output --------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_output():
    """A statement with no print produces ``(no output)``."""

    async def body(session):
        result = await session.call_tool("run_python", {"code": "y = 1"})
        assert result.content[0].text == "(no output)"

    await _run_with_session(body)


# -- Timeout override --------------------------------------------------------


@pytest.mark.asyncio
async def test_timeout_override():
    """The ``timeout_ms`` parameter causes long-running code to be killed."""

    async def body(session):
        result = await session.call_tool(
            "run_python",
            {"code": "while True: pass", "timeout_ms": 500},
        )
        text = result.content[0].text
        assert "timed out" in text.lower() or "timeout" in text.lower()

    await _run_with_session(body)
