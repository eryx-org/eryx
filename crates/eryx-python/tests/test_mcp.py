"""Tests for MCP (Model Context Protocol) integration."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from unittest.mock import patch

import eryx
import pytest

from eryx.mcp import (
    MCPError,
    MCPManager,
    StdioMCPClient,
    _expand_env_vars,
    _parse_config_file,
    discover_servers,
)

# Path to the mock MCP server script
MOCK_SERVER = str(Path(__file__).parent / "mock_mcp_server.py")


# =============================================================================
# Config Parsing Tests
# =============================================================================


class TestConfigParsing:
    """Tests for MCP config file discovery and parsing."""

    def test_parse_mcp_json_format(self, tmp_path):
        """Test parsing .mcp.json project config format."""
        config = tmp_path / ".mcp.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "test-server": {
                            "command": "python",
                            "args": ["server.py"],
                            "env": {"API_KEY": "test"},
                        }
                    }
                }
            )
        )

        result = _parse_config_file(config)
        assert "test-server" in result
        assert result["test-server"]["type"] == "stdio"
        assert result["test-server"]["command"] == "python"
        assert result["test-server"]["args"] == ["server.py"]
        assert result["test-server"]["env"] == {"API_KEY": "test"}

    def test_parse_claude_json_format(self, tmp_path):
        """Test parsing ~/.claude.json user config format."""
        config = tmp_path / ".claude.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "github": {
                            "command": "npx",
                            "args": ["-y", "@github/mcp-server"],
                            "env": {},
                        }
                    },
                    "otherSettings": {"key": "value"},
                }
            )
        )

        result = _parse_config_file(config)
        assert "github" in result
        assert result["github"]["command"] == "npx"

    def test_parse_config_with_type_field(self, tmp_path):
        """Test parsing config with explicit type: stdio."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "local": {
                            "type": "stdio",
                            "command": "/usr/local/bin/mcp-server",
                            "args": ["--port", "8080"],
                        }
                    }
                }
            )
        )

        result = _parse_config_file(config)
        assert "local" in result
        assert result["local"]["type"] == "stdio"

    def test_parse_config_skips_http_servers(self, tmp_path):
        """Test that HTTP servers are skipped (not yet supported)."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "remote": {
                            "type": "http",
                            "url": "https://example.com/mcp",
                        }
                    }
                }
            )
        )

        result = _parse_config_file(config)
        assert "remote" not in result

    def test_parse_config_skips_missing_command(self, tmp_path):
        """Test that servers without a command are skipped."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "bad": {"args": ["--flag"]},
                    }
                }
            )
        )

        result = _parse_config_file(config)
        assert "bad" not in result

    def test_parse_config_empty_mcp_servers(self, tmp_path):
        """Test parsing config with empty mcpServers."""
        config = tmp_path / "config.json"
        config.write_text(json.dumps({"mcpServers": {}}))

        result = _parse_config_file(config)
        assert result == {}

    def test_parse_config_no_mcp_servers_key(self, tmp_path):
        """Test parsing config without mcpServers key."""
        config = tmp_path / "config.json"
        config.write_text(json.dumps({"other": "data"}))

        result = _parse_config_file(config)
        assert result == {}

    def test_parse_config_multiple_servers(self, tmp_path):
        """Test parsing config with multiple servers."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "server-a": {"command": "cmd-a"},
                        "server-b": {"command": "cmd-b", "args": ["--flag"]},
                        "server-c": {
                            "command": "cmd-c",
                            "env": {"KEY": "val"},
                        },
                    }
                }
            )
        )

        result = _parse_config_file(config)
        assert len(result) == 3
        assert result["server-a"]["command"] == "cmd-a"
        assert result["server-b"]["args"] == ["--flag"]
        assert result["server-c"]["env"] == {"KEY": "val"}

    def test_discover_servers_from_cwd(self, tmp_path, monkeypatch):
        """Test that discover_servers finds .mcp.json in cwd."""
        config = tmp_path / ".mcp.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "local": {"command": "local-server"},
                    }
                }
            )
        )
        monkeypatch.chdir(tmp_path)

        result = discover_servers()
        assert "local" in result

    def test_discover_servers_explicit_paths(self, tmp_path):
        """Test discover_servers with explicit config paths."""
        config = tmp_path / "custom.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "custom": {"command": "custom-server"},
                    }
                }
            )
        )

        result = discover_servers(config_paths=[config])
        assert "custom" in result

    def test_discover_servers_precedence(self, tmp_path):
        """Test that earlier config files take precedence."""
        config1 = tmp_path / "first.json"
        config1.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "dup": {"command": "first-cmd"},
                    }
                }
            )
        )

        config2 = tmp_path / "second.json"
        config2.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "dup": {"command": "second-cmd"},
                    }
                }
            )
        )

        result = discover_servers(config_paths=[config1, config2])
        assert result["dup"]["command"] == "first-cmd"


# =============================================================================
# Environment Variable Expansion Tests
# =============================================================================


class TestEnvVarExpansion:
    """Tests for environment variable expansion in config values."""

    def test_expand_simple_var(self, monkeypatch):
        monkeypatch.setenv("TEST_VAR", "hello")
        assert _expand_env_vars("${TEST_VAR}") == "hello"

    def test_expand_var_with_default(self):
        assert _expand_env_vars("${NONEXISTENT_VAR:-fallback}") == "fallback"

    def test_expand_var_set_ignores_default(self, monkeypatch):
        monkeypatch.setenv("TEST_VAR", "actual")
        assert _expand_env_vars("${TEST_VAR:-fallback}") == "actual"

    def test_expand_multiple_vars(self, monkeypatch):
        monkeypatch.setenv("A", "hello")
        monkeypatch.setenv("B", "world")
        assert _expand_env_vars("${A} ${B}") == "hello world"

    def test_expand_no_vars(self):
        assert _expand_env_vars("plain string") == "plain string"

    def test_expand_unset_var_no_default(self):
        result = _expand_env_vars("${DEFINITELY_NOT_SET_12345}")
        assert result == "${DEFINITELY_NOT_SET_12345}"


# =============================================================================
# StdioMCPClient Tests
# =============================================================================


class TestStdioMCPClient:
    """Tests for the stdio MCP client."""

    def test_connect_and_list_tools(self):
        """Test connecting to a mock MCP server and listing tools."""
        client = StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        )
        try:
            client.connect(timeout=10.0)
            assert len(client.tools) == 5
            tool_names = [t["name"] for t in client.tools]
            assert "echo" in tool_names
            assert "add" in tool_names
            assert "greet" in tool_names
            assert "fail" in tool_names
            assert "json_result" in tool_names
        finally:
            client.close()

    def test_server_info(self):
        """Test that server info is populated after connect."""
        client = StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        )
        try:
            client.connect(timeout=10.0)
            info = client.server_info
            assert info.get("serverInfo", {}).get("name") == "mock-mcp-server"
        finally:
            client.close()

    def test_call_echo_tool(self):
        """Test calling the echo tool."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            result = client.call_tool("echo", {"message": "hello world"})
            assert result == "hello world"

    def test_call_add_tool(self):
        """Test calling the add tool."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            result = client.call_tool("add", {"a": 3, "b": 5})
            assert result == {"sum": 8}

    def test_call_greet_tool(self):
        """Test calling the greet tool."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            result = client.call_tool("greet", {"name": "Eryx"})
            assert result == "Hello, Eryx!"

    def test_call_json_result_tool(self):
        """Test calling a tool that returns structured JSON."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            result = client.call_tool(
                "json_result", {"key": "color", "value": "blue"}
            )
            assert result == {"color": "blue"}

    def test_call_failing_tool(self):
        """Test that a tool returning isError=true raises MCPError."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            with pytest.raises(MCPError, match="error"):
                client.call_tool("fail", {})

    def test_connect_nonexistent_command(self):
        """Test that connecting to a nonexistent command raises MCPError."""
        client = StdioMCPClient(
            name="test",
            command="/nonexistent/binary",
        )
        with pytest.raises(MCPError, match="command not found"):
            client.connect(timeout=5.0)

    def test_context_manager(self):
        """Test using StdioMCPClient as a context manager."""
        with StdioMCPClient(
            name="test",
            command=sys.executable,
            args=[MOCK_SERVER],
        ) as client:
            client.connect(timeout=10.0)
            assert len(client.tools) > 0
        # After exiting context, process should be terminated


# =============================================================================
# MCPManager Tests
# =============================================================================


class TestMCPManager:
    """Tests for the MCPManager orchestrator."""

    def test_discover_and_connect(self, tmp_path):
        """Test full discovery and connection flow."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            assert "mock" in manager.server_names

            manager.connect_all()
            assert "mock" in manager.connected_servers

            tools = manager.all_tools()
            assert len(tools) == 5
            assert all(t["_server"] == "mock" for t in tools)

    def test_as_callbacks(self, tmp_path):
        """Test converting MCP tools to eryx callbacks."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            assert len(callbacks) == 5
            names = [cb["name"] for cb in callbacks]
            assert "mcp.mock.echo" in names
            assert "mcp.mock.add" in names
            assert "mcp.mock.greet" in names

            # Check callback structure
            echo_cb = next(cb for cb in callbacks if cb["name"] == "mcp.mock.echo")
            assert callable(echo_cb["fn"])
            assert "description" in echo_cb
            assert "[MCP: mock]" in echo_cb["description"]
            assert "schema" in echo_cb

    def test_callback_invocation(self, tmp_path):
        """Test that callbacks actually work when invoked."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            # Find and invoke the add callback
            add_cb = next(cb for cb in callbacks if cb["name"] == "mcp.mock.add")
            result = add_cb["fn"](a=10, b=20)
            assert result == {"sum": 30}

            # Find and invoke the greet callback
            greet_cb = next(
                cb for cb in callbacks if cb["name"] == "mcp.mock.greet"
            )
            result = greet_cb["fn"](name="Test")
            assert result == "Hello, Test!"

    def test_no_servers_found(self, tmp_path, monkeypatch):
        """Test handling when no MCP servers are found."""
        empty_config = tmp_path / "empty.json"
        empty_config.write_text(json.dumps({}))

        with MCPManager(config_paths=[empty_config]) as manager:
            manager.discover()
            assert manager.server_names == []

    def test_failed_server_skipped(self, tmp_path):
        """Test that a server that fails to connect is skipped."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "bad": {"command": "/nonexistent/binary"},
                        "good": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        },
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            # Only the good server should be connected
            assert "good" in manager.connected_servers
            assert "bad" not in manager.connected_servers


# =============================================================================
# Sandbox Integration Tests
# =============================================================================


class TestSandboxMCPIntegration:
    """Tests for MCP tools used as sandbox callbacks."""

    def test_sandbox_with_mcp_callbacks_dotted(self, tmp_path):
        """Test running sandbox code using dotted namespace syntax (mcp.mock.add)."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            # MCP tools are accessible via dotted namespace: mcp.mock.add(a=7, b=3)
            sandbox = eryx.Sandbox(callbacks=callbacks)
            result = sandbox.execute(
                """
result = await mcp.mock.add(a=7, b=3)
print(result)
"""
            )
            assert "sum" in result.stdout
            assert "10" in result.stdout

    def test_sandbox_with_mcp_callbacks_invoke(self, tmp_path):
        """Test calling MCP tools via invoke() with kwargs."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            sandbox = eryx.Sandbox(callbacks=callbacks)
            result = sandbox.execute(
                """
result = await invoke("mcp.mock.add", a=7, b=3)
print(result)
"""
            )
            assert "sum" in result.stdout
            assert "10" in result.stdout

    def test_sandbox_greet_tool(self, tmp_path):
        """Test the greet MCP tool from sandbox."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            sandbox = eryx.Sandbox(callbacks=callbacks)
            result = sandbox.execute(
                """
result = await mcp.mock.greet(name="World")
print(result)
"""
            )
            assert "Hello, World!" in result.stdout

    def test_sandbox_list_mcp_callbacks(self, tmp_path):
        """Test that list_callbacks() shows MCP tools."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            sandbox = eryx.Sandbox(callbacks=callbacks)
            result = sandbox.execute(
                """
cbs = list_callbacks()
for cb in cbs:
    print(cb['name'])
"""
            )
            assert "mcp.mock.echo" in result.stdout
            assert "mcp.mock.add" in result.stdout

    def test_sandbox_mcp_tool_error_handling(self, tmp_path):
        """Test that MCP tool errors are propagated to sandbox code."""
        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        with MCPManager(config_paths=[config]) as manager:
            manager.discover()
            manager.connect_all()
            callbacks = manager.as_callbacks()

            sandbox = eryx.Sandbox(callbacks=callbacks)
            result = sandbox.execute(
                """
try:
    await invoke("mcp.mock.fail", '{}')
    print("SHOULD_NOT_REACH")
except Exception as e:
    print(f"caught: {type(e).__name__}")
"""
            )
            assert "SHOULD_NOT_REACH" not in result.stdout
            assert "caught:" in result.stdout


# =============================================================================
# CLI Integration Tests
# =============================================================================


class TestCLIMCPIntegration:
    """Tests for MCP integration in the CLI."""

    def test_mcp_flag_accepted(self, capsys):
        """Test that --mcp flag is accepted without error."""
        from eryx.__main__ import main

        # With no config files, --mcp should just print a warning
        result = main(["--mcp", "-c", 'print("hello")'])
        captured = capsys.readouterr()
        # Should either succeed with MCP or succeed without (no config found)
        assert result == 0 or "no MCP servers" in captured.err

    def test_mcp_config_flag(self, tmp_path, capsys):
        """Test that --mcp-config points to a specific config file."""
        from eryx.__main__ import main

        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        result = main(
            [
                "--mcp-config",
                str(config),
                "-c",
                'cbs = list_callbacks(); print(len(cbs))',
            ]
        )
        captured = capsys.readouterr()
        assert result == 0
        assert "MCP tool(s)" in captured.err
        assert "5" in captured.out

    def test_mcp_config_tool_call(self, tmp_path, capsys):
        """Test calling an MCP tool through the CLI."""
        from eryx.__main__ import main

        config = tmp_path / "config.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        "mock": {
                            "command": sys.executable,
                            "args": [MOCK_SERVER],
                        }
                    }
                }
            )
        )

        result = main(
            [
                "--mcp-config",
                str(config),
                "-c",
                "r = await mcp.mock.add(a=100, b=200); print(r)",
            ]
        )
        captured = capsys.readouterr()
        assert result == 0
        assert "300" in captured.out

    def test_mcp_empty_config(self, tmp_path, capsys):
        """Test --mcp-config with a config that has no servers."""
        from eryx.__main__ import main

        config = tmp_path / "empty.json"
        config.write_text(json.dumps({"mcpServers": {}}))

        result = main(
            ["--mcp-config", str(config), "-c", 'print("ok")']
        )
        captured = capsys.readouterr()
        assert result == 0
        assert "no MCP servers" in captured.err
        assert "ok" in captured.out
