"""Tests for MCP (Model Context Protocol) integration."""

from __future__ import annotations

import json
import os
import sys
import textwrap
from pathlib import Path
from unittest.mock import patch

import eryx
import pytest

# Path to the mock MCP server script
MOCK_SERVER = str(Path(__file__).parent / "mock_mcp_server.py")


# =============================================================================
# Config Discovery Tests
# =============================================================================


class TestConfigDiscovery:
    """Tests for MCP config file discovery and parsing."""

    def test_discover_from_mcp_json(self, tmp_path):
        """Test discovering servers from .mcp.json."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "test": {
                    "command": "some-cmd",
                    "args": ["--flag"],
                    "env": {"KEY": "val"},
                }
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "test" in servers
        assert servers["test"]["command"] == "some-cmd"
        assert servers["test"]["args"] == ["--flag"]
        assert servers["test"]["env"] == {"KEY": "val"}

    def test_discover_skips_disabled(self, tmp_path):
        """Test that disabled servers are skipped."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "enabled": {"command": "cmd1"},
                "disabled": {"command": "cmd2", "disabled": True},
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "enabled" in servers
        assert "disabled" not in servers

    def test_discover_skips_enabled_false(self, tmp_path):
        """Test that servers with enabled=false are skipped (Codex style)."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "active": {"command": "cmd1"},
                "inactive": {"command": "cmd2", "enabled": False},
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "active" in servers
        assert "inactive" not in servers

    def test_discover_skips_non_stdio(self, tmp_path):
        """Test that non-stdio servers are skipped."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "stdio": {"command": "cmd1"},
                "sse": {"command": "cmd2", "type": "sse"},
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "stdio" in servers
        assert "sse" not in servers

    def test_discover_later_configs_override(self, tmp_path):
        """Test that later config files override earlier ones."""
        from eryx.mcp import discover_servers

        config1 = {"mcpServers": {"test": {"command": "cmd1"}}}
        config2 = {"mcpServers": {"test": {"command": "cmd2"}}}

        f1 = tmp_path / "first.json"
        f2 = tmp_path / "second.json"
        f1.write_text(json.dumps(config1))
        f2.write_text(json.dumps(config2))

        servers = discover_servers(config_paths=[f1, f2])
        assert servers["test"]["command"] == "cmd2"

    def test_discover_missing_config(self, tmp_path):
        """Test that missing config files are silently skipped."""
        from eryx.mcp import discover_servers

        servers = discover_servers(config_paths=[tmp_path / "nonexistent.json"])
        assert servers == {}

    def test_discover_invalid_json(self, tmp_path):
        """Test that invalid JSON files are silently skipped."""
        from eryx.mcp import discover_servers

        config_file = tmp_path / ".mcp.json"
        config_file.write_text("not valid json{{{")

        servers = discover_servers(config_paths=[config_file])
        assert servers == {}

    def test_discover_claude_json_format(self, tmp_path):
        """Test discovering from ~/.claude.json format."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@anthropic/mcp-server-filesystem", "."],
                }
            }
        }
        config_file = tmp_path / ".claude.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "filesystem" in servers
        assert servers["filesystem"]["command"] == "npx"

    def test_discover_no_command_skipped(self, tmp_path):
        """Test that entries without a command are skipped."""
        from eryx.mcp import discover_servers

        config = {
            "mcpServers": {
                "no_cmd": {"args": ["--flag"]},
                "has_cmd": {"command": "cmd1"},
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        servers = discover_servers(config_paths=[config_file])
        assert "no_cmd" not in servers
        assert "has_cmd" in servers


class TestMultiIDEDiscovery:
    """Tests for discovering MCP servers from various IDE config formats."""

    def test_vscode_servers_key(self, tmp_path):
        """Test VS Code uses 'servers' key instead of 'mcpServers'."""
        from eryx.mcp import _extract_stdio_servers

        data = {
            "servers": {
                "memory": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-memory"],
                }
            }
        }
        servers = _extract_stdio_servers(data, "servers")
        assert "memory" in servers
        assert servers["memory"]["command"] == "npx"

    def test_zed_context_servers_key(self, tmp_path):
        """Test Zed uses 'context_servers' key."""
        from eryx.mcp import _extract_stdio_servers

        data = {
            "context_servers": {
                "my_server": {
                    "command": "node",
                    "args": ["/path/to/server.js"],
                    "env": {"API_KEY": "test"},
                }
            }
        }
        servers = _extract_stdio_servers(data, "context_servers")
        assert "my_server" in servers
        assert servers["my_server"]["command"] == "node"
        assert servers["my_server"]["env"] == {"API_KEY": "test"}

    def test_codex_toml_format(self, tmp_path):
        """Test Codex TOML config format."""
        from eryx.mcp import _read_toml, _extract_stdio_servers

        toml_content = """\
[mcp_servers.my_server]
command = "npx"
args = ["-y", "@some/mcp-server"]

[mcp_servers.my_server.env]
API_KEY = "test-key"

[mcp_servers.disabled_server]
command = "other-cmd"
enabled = false
"""
        toml_file = tmp_path / "config.toml"
        toml_file.write_text(toml_content)

        data = _read_toml(toml_file)
        assert data is not None

        servers = _extract_stdio_servers(data, "mcp_servers")
        assert "my_server" in servers
        assert servers["my_server"]["command"] == "npx"
        assert servers["my_server"]["args"] == ["-y", "@some/mcp-server"]
        assert servers["my_server"]["env"] == {"API_KEY": "test-key"}
        assert "disabled_server" not in servers

    def test_codex_toml_invalid(self, tmp_path):
        """Test that invalid TOML files are silently skipped."""
        from eryx.mcp import _read_toml

        toml_file = tmp_path / "bad.toml"
        toml_file.write_text("not valid [[[toml")

        assert _read_toml(toml_file) is None

    def test_cursor_config(self, tmp_path):
        """Test Cursor config format (same key as Claude)."""
        from eryx.mcp import _extract_stdio_servers

        data = {
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@anthropic/mcp-server-github"],
                    "env": {"GITHUB_TOKEN": "ghp_xxx"},
                }
            }
        }
        servers = _extract_stdio_servers(data, "mcpServers")
        assert "github" in servers
        assert servers["github"]["env"] == {"GITHUB_TOKEN": "ghp_xxx"}

    def test_windsurf_config(self, tmp_path):
        """Test Windsurf config (mcpServers key, skips remote servers)."""
        from eryx.mcp import _extract_stdio_servers

        data = {
            "mcpServers": {
                "local": {"command": "my-server", "args": ["--local"]},
                "remote": {"serverUrl": "https://mcp.example.com/mcp"},
            }
        }
        servers = _extract_stdio_servers(data, "mcpServers")
        assert "local" in servers
        assert "remote" not in servers

    def test_gemini_config(self, tmp_path):
        """Test Gemini CLI config format."""
        from eryx.mcp import _extract_stdio_servers

        data = {
            "mcpServers": {
                "local_tool": {
                    "command": "/usr/local/bin/my-tool",
                    "args": ["--arg1"],
                },
                "http_tool": {
                    "httpUrl": "https://mcp.example.com/endpoint",
                },
            }
        }
        servers = _extract_stdio_servers(data, "mcpServers")
        assert "local_tool" in servers
        assert "http_tool" not in servers

    def test_default_sources_structure(self):
        """Test that _default_sources returns expected structure."""
        from eryx.mcp import _default_sources

        sources = _default_sources()
        # Should have both global and project sources
        assert len(sources) >= 10

        # Check some expected entries exist
        paths = [str(s.path) for s in sources]
        keys = [s.key for s in sources]
        fmts = [s.fmt for s in sources]

        # Claude Code global
        assert any(".claude.json" in p for p in paths)
        # Cursor
        assert any(".cursor" in p and "mcp.json" in p for p in paths)
        # VS Code
        assert any(".vscode" in p for p in paths)
        # Zed
        assert any("zed" in p for p in paths)
        assert "context_servers" in keys
        # Gemini
        assert any(".gemini" in p for p in paths)
        # Codex TOML
        assert any(".codex" in p for p in paths)
        assert "toml" in fmts
        # VS Code uses "servers"
        assert "servers" in keys


class TestEnvVarExpansion:
    """Tests for environment variable expansion in MCP config."""

    def test_expand_simple_var(self):
        from eryx.mcp import _expand_env_vars

        with patch.dict(os.environ, {"MY_VAR": "hello"}):
            assert _expand_env_vars("$MY_VAR") == "hello"

    def test_expand_braced_var(self):
        from eryx.mcp import _expand_env_vars

        with patch.dict(os.environ, {"MY_VAR": "hello"}):
            assert _expand_env_vars("${MY_VAR}") == "hello"

    def test_expand_with_default(self):
        from eryx.mcp import _expand_env_vars

        # Unset variable uses default
        env = os.environ.copy()
        env.pop("UNSET_VAR", None)
        with patch.dict(os.environ, env, clear=True):
            assert _expand_env_vars("${UNSET_VAR:-fallback}") == "fallback"

    def test_expand_set_var_ignores_default(self):
        from eryx.mcp import _expand_env_vars

        with patch.dict(os.environ, {"SET_VAR": "real_value"}):
            assert _expand_env_vars("${SET_VAR:-fallback}") == "real_value"

    def test_expand_missing_var_empty(self):
        from eryx.mcp import _expand_env_vars

        env = os.environ.copy()
        env.pop("MISSING", None)
        with patch.dict(os.environ, env, clear=True):
            assert _expand_env_vars("$MISSING") == ""

    def test_expand_in_url(self):
        from eryx.mcp import _expand_env_vars

        with patch.dict(os.environ, {"API_KEY": "sk-123"}):
            result = _expand_env_vars("https://api.example.com?key=$API_KEY")
            assert result == "https://api.example.com?key=sk-123"


# =============================================================================
# MCPManager Unit Tests
# =============================================================================


class TestMCPManager:
    """Tests for the MCPManager Rust class."""

    def test_create_empty(self):
        """Test creating an empty MCPManager."""
        manager = eryx.MCPManager()
        assert manager.server_names == []
        assert manager.list_tools() == []

    def test_repr_empty(self):
        """Test repr of empty manager."""
        manager = eryx.MCPManager()
        r = repr(manager)
        assert "MCPManager" in r
        assert "tools=0" in r

    def test_connect_mock_server(self):
        """Test connecting to the mock MCP server."""
        manager = eryx.MCPManager()
        tool_count = manager.connect(
            "mock",
            sys.executable,
            [MOCK_SERVER],
            {},
            10.0,
        )
        assert tool_count == 2
        assert "mock" in manager.server_names

        tools = manager.list_tools()
        assert len(tools) == 2
        tool_names = [t["name"] for t in tools]
        assert "mcp.mock.echo" in tool_names
        assert "mcp.mock.add" in tool_names

        manager.close()

    def test_connect_failure_bad_command(self):
        """Test that connecting to a nonexistent command fails."""
        manager = eryx.MCPManager()
        with pytest.raises(eryx.InitializationError):
            manager.connect("bad", "/nonexistent/command", [], {}, 5.0)

    def test_connect_multiple_servers(self):
        """Test connecting to multiple mock servers."""
        manager = eryx.MCPManager()
        manager.connect("server1", sys.executable, [MOCK_SERVER], {}, 10.0)
        manager.connect("server2", sys.executable, [MOCK_SERVER], {}, 10.0)

        assert len(manager.server_names) == 2
        assert "server1" in manager.server_names
        assert "server2" in manager.server_names

        tools = manager.list_tools()
        # 2 tools per server × 2 servers
        assert len(tools) == 4

        manager.close()

    def test_close_idempotent(self):
        """Test that close() can be called multiple times safely."""
        manager = eryx.MCPManager()
        manager.connect("mock", sys.executable, [MOCK_SERVER], {}, 10.0)
        manager.close()
        manager.close()  # Should not raise

    def test_repr_with_connections(self):
        """Test repr after connecting."""
        manager = eryx.MCPManager()
        manager.connect("testserver", sys.executable, [MOCK_SERVER], {}, 10.0)
        r = repr(manager)
        assert "testserver" in r
        assert "tools=2" in r
        manager.close()

    def test_tool_descriptions(self):
        """Test that tool descriptions are populated."""
        manager = eryx.MCPManager()
        manager.connect("mock", sys.executable, [MOCK_SERVER], {}, 10.0)

        tools = manager.list_tools()
        echo_tool = next(t for t in tools if t["name"] == "mcp.mock.echo")
        assert "echo" in echo_tool["description"].lower()

        add_tool = next(t for t in tools if t["name"] == "mcp.mock.add")
        assert "add" in add_tool["description"].lower()

        manager.close()

    def test_tool_schemas(self):
        """Test that tool schemas are populated."""
        manager = eryx.MCPManager()
        manager.connect("mock", sys.executable, [MOCK_SERVER], {}, 10.0)

        tools = manager.list_tools()
        echo_tool = next(t for t in tools if t["name"] == "mcp.mock.echo")
        schema = echo_tool["schema"]
        assert "properties" in schema
        assert "message" in schema["properties"]

        manager.close()


# =============================================================================
# Sandbox Integration Tests
# =============================================================================


class TestMCPSandboxIntegration:
    """Tests for MCP tools used via Sandbox."""

    @pytest.fixture
    def mcp_manager(self):
        """Create an MCPManager connected to the mock server."""
        manager = eryx.MCPManager()
        manager.connect("mock", sys.executable, [MOCK_SERVER], {}, 10.0)
        yield manager
        manager.close()

    def test_echo_tool(self, mcp_manager):
        """Test calling the echo MCP tool from sandbox."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            'r = await mcp.mock.echo(message="hello world"); print(r["text"])'
        )
        assert "hello world" in result.stdout

    def test_add_tool(self, mcp_manager):
        """Test calling the add MCP tool from sandbox."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            'r = await mcp.mock.add(a=3, b=5); print(r["result"])'
        )
        assert "8" in result.stdout

    def test_list_callbacks_includes_mcp(self, mcp_manager):
        """Test that list_callbacks() shows MCP tools."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            textwrap.dedent("""\
            callbacks = list_callbacks()
            names = [c['name'] for c in callbacks]
            for n in sorted(names):
                print(n)
            """)
        )
        assert "mcp.mock.echo" in result.stdout
        assert "mcp.mock.add" in result.stdout

    def test_mcp_with_python_callbacks(self, mcp_manager):
        """Test MCP tools alongside Python callbacks."""

        def py_double(x: int):
            return {"result": x * 2}

        sandbox = eryx.Sandbox(
            mcp=mcp_manager,
            callbacks=[
                {"name": "py_double", "fn": py_double, "description": "Doubles x"}
            ],
        )
        result = sandbox.execute(
            textwrap.dedent("""\
            mcp_r = await mcp.mock.add(a=10, b=20)
            py_r = await py_double(x=15)
            print(f"mcp={mcp_r['result']}, py={py_r['result']}")
            """)
        )
        assert "mcp=30" in result.stdout
        assert "py=30" in result.stdout

    def test_invoke_style(self, mcp_manager):
        """Test calling MCP tool via invoke() style."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            textwrap.dedent("""\
            r = await invoke("mcp.mock.echo", message="invoked")
            print(r["text"])
            """)
        )
        assert "invoked" in result.stdout

    def test_dict_access_style(self, mcp_manager):
        """Test calling MCP tool via dict-access on namespace."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            textwrap.dedent("""\
            r = await mcp["mock"]["echo"](message="dict-style")
            print(r["text"])
            """)
        )
        assert "dict-style" in result.stdout

    def test_dict_access_hyphenated_server(self):
        """Test dict-access with hyphenated server name (invalid Python identifier)."""
        manager = eryx.MCPManager()
        manager.connect("my-server", sys.executable, [MOCK_SERVER], {}, 10.0)
        try:
            sandbox = eryx.Sandbox(mcp=manager)
            result = sandbox.execute(
                textwrap.dedent("""\
                r = await mcp["my-server"]["echo"](message="hyphen-test")
                print(r["text"])
                """)
            )
            assert "hyphen-test" in result.stdout
        finally:
            manager.close()

    def test_dict_access_mixed_with_dotted(self, mcp_manager):
        """Test mixing dict-access and dotted syntax."""
        sandbox = eryx.Sandbox(mcp=mcp_manager)
        result = sandbox.execute(
            textwrap.dedent("""\
            r = await mcp["mock"].echo(message="mixed-style")
            print(r["text"])
            """)
        )
        assert "mixed-style" in result.stdout


# =============================================================================
# Session Integration Tests
# =============================================================================


class TestMCPSessionIntegration:
    """Tests for MCP tools used via Session."""

    @pytest.fixture
    def mcp_manager(self):
        """Create an MCPManager connected to the mock server."""
        manager = eryx.MCPManager()
        manager.connect("mock", sys.executable, [MOCK_SERVER], {}, 10.0)
        yield manager
        manager.close()

    def test_session_mcp_tool(self, mcp_manager):
        """Test calling MCP tool from a Session."""
        session = eryx.Session(mcp=mcp_manager)
        result = session.execute(
            'r = await mcp.mock.echo(message="session test"); print(r["text"])'
        )
        assert "session test" in result.stdout

    def test_session_state_with_mcp(self, mcp_manager):
        """Test that session state persists while using MCP tools."""
        session = eryx.Session(mcp=mcp_manager)
        session.execute("x = 10")
        result = session.execute(
            textwrap.dedent("""\
            r = await mcp.mock.add(a=x, b=5)
            y = r["result"]
            print(y)
            """)
        )
        assert "15" in result.stdout

        # State persists
        result = session.execute("print(y)")
        assert "15" in result.stdout

    def test_session_mcp_with_callbacks(self, mcp_manager):
        """Test Session with both MCP and Python callbacks."""

        def py_greet(name: str):
            return {"greeting": f"Hello, {name}!"}

        session = eryx.Session(
            mcp=mcp_manager,
            callbacks=[
                {"name": "py_greet", "fn": py_greet, "description": "Greets someone"}
            ],
        )
        result = session.execute(
            textwrap.dedent("""\
            echo_r = await mcp.mock.echo(message="world")
            greet_r = await py_greet(name="world")
            print(f"echo={echo_r['text']}, greet={greet_r['greeting']}")
            """)
        )
        assert "echo=world" in result.stdout
        assert "greet=Hello, world!" in result.stdout


# =============================================================================
# CLI Integration Tests
# =============================================================================


class TestMCPCLI:
    """Tests for MCP CLI flags."""

    def test_mcp_config_flag(self, tmp_path):
        """Test --mcp-config flag with the mock server."""
        config = {
            "mcpServers": {
                "mock": {
                    "command": sys.executable,
                    "args": [MOCK_SERVER],
                }
            }
        }
        config_file = tmp_path / ".mcp.json"
        config_file.write_text(json.dumps(config))

        from eryx.__main__ import main

        code = 'r = await mcp.mock.echo(message="cli-test"); print(r["text"])'
        # Capture stdout
        import io

        old_stdout = sys.stdout
        old_stderr = sys.stderr
        sys.stdout = io.StringIO()
        sys.stderr = io.StringIO()
        try:
            exit_code = main(["-c", code, "--mcp-config", str(config_file)])
            stdout_val = sys.stdout.getvalue()
            stderr_val = sys.stderr.getvalue()
        finally:
            sys.stdout = old_stdout
            sys.stderr = old_stderr

        assert exit_code == 0, f"exit_code={exit_code}, stderr={stderr_val}"
        assert "cli-test" in stdout_val
