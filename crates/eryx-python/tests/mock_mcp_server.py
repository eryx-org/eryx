#!/usr/bin/env python3
"""Mock MCP server for testing.

A minimal MCP server that communicates via JSON-RPC 2.0 over stdio.
Supports the initialize handshake, tools/list, and tools/call.

Tools:
  - echo: Returns its arguments back
  - add: Adds two numbers
  - greet: Returns a greeting message
  - fail: Always returns an error
"""

import json
import sys


TOOLS = [
    {
        "name": "echo",
        "description": "Echoes back the input arguments",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "Message to echo"},
            },
            "required": ["message"],
        },
    },
    {
        "name": "add",
        "description": "Adds two numbers together",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "number", "description": "First number"},
                "b": {"type": "number", "description": "Second number"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "greet",
        "description": "Returns a greeting message",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name to greet"},
            },
            "required": ["name"],
        },
    },
    {
        "name": "fail",
        "description": "Always returns an error",
        "inputSchema": {
            "type": "object",
            "properties": {},
        },
    },
    {
        "name": "json_result",
        "description": "Returns structured JSON as text content",
        "inputSchema": {
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {"type": "string"},
            },
            "required": ["key", "value"],
        },
    },
]


def handle_request(request):
    """Handle a JSON-RPC request and return a response."""
    method = request.get("method")
    params = request.get("params", {})
    msg_id = request.get("id")

    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp-server", "version": "0.1.0"},
            },
        }

    if method == "notifications/initialized":
        # Notification, no response needed
        return None

    if method == "tools/list":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {"tools": TOOLS},
        }

    if method == "tools/call":
        tool_name = params.get("name")
        arguments = params.get("arguments", {})
        return _call_tool(msg_id, tool_name, arguments)

    # Unknown method
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "error": {
            "code": -32601,
            "message": f"Method not found: {method}",
        },
    }


def _call_tool(msg_id, tool_name, arguments):
    """Execute a tool call and return the response."""
    if tool_name == "echo":
        message = arguments.get("message", "")
        return _success(msg_id, message)

    if tool_name == "add":
        a = arguments.get("a", 0)
        b = arguments.get("b", 0)
        return _success(msg_id, json.dumps({"sum": a + b}))

    if tool_name == "greet":
        name = arguments.get("name", "World")
        return _success(msg_id, f"Hello, {name}!")

    if tool_name == "fail":
        return _error_result(msg_id, "This tool always fails")

    if tool_name == "json_result":
        key = arguments.get("key", "k")
        value = arguments.get("value", "v")
        return _success(msg_id, json.dumps({key: value}))

    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "error": {
            "code": -32602,
            "message": f"Unknown tool: {tool_name}",
        },
    }


def _success(msg_id, text):
    """Create a successful tool result."""
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "content": [{"type": "text", "text": text}],
        },
    }


def _error_result(msg_id, text):
    """Create an error tool result (isError=true, not a JSON-RPC error)."""
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": True,
        },
    }


def main():
    """Main loop: read JSON-RPC messages from stdin, write responses to stdout."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            sys.stderr.write(f"Invalid JSON: {line}\n")
            continue

        response = handle_request(request)
        if response is not None:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()
