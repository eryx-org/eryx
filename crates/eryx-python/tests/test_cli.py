"""Tests for the eryx CLI (__main__.py)."""

import subprocess
import sys
import textwrap
from unittest.mock import patch

import pytest

from eryx.__main__ import main


class TestCliCommandExecution:
    """Tests for -c flag (command execution)."""

    def test_execute_simple_command(self):
        result = main(["-c", 'print("hello")'])
        assert result == 0

    def test_execute_command_with_output(self, capsys):
        main(["-c", 'print("hello world")'])
        captured = capsys.readouterr()
        assert "hello world" in captured.out

    def test_execute_multiline_command(self, capsys):
        main(["-c", "x = 2 + 3\nprint(x)"])
        captured = capsys.readouterr()
        assert "5" in captured.out

    def test_execute_command_with_error(self, capsys):
        result = main(["-c", "raise ValueError('boom')"])
        assert result == 1
        captured = capsys.readouterr()
        assert "ValueError" in captured.err

    def test_execute_empty_command(self):
        result = main(["-c", ""])
        assert result == 0


class TestCliScriptExecution:
    """Tests for script file execution."""

    def test_execute_script_file(self, tmp_path, capsys):
        script = tmp_path / "test.py"
        script.write_text('print("from file")')
        result = main([str(script)])
        assert result == 0
        captured = capsys.readouterr()
        assert "from file" in captured.out

    def test_execute_nonexistent_file(self, capsys):
        result = main(["/nonexistent/path.py"])
        assert result == 1
        captured = capsys.readouterr()
        assert "No such file" in captured.err

    def test_execute_multiline_script(self, tmp_path, capsys):
        script = tmp_path / "multi.py"
        script.write_text(textwrap.dedent("""\
            for i in range(3):
                print(f"line {i}")
        """))
        result = main([str(script)])
        assert result == 0
        captured = capsys.readouterr()
        assert "line 0" in captured.out
        assert "line 1" in captured.out
        assert "line 2" in captured.out

    def test_execute_stdin_dash(self, capsys):
        with patch("sys.stdin") as mock_stdin:
            mock_stdin.read.return_value = 'print("from stdin")'
            result = main(["-"])
            assert result == 0
            captured = capsys.readouterr()
            assert "from stdin" in captured.out


class TestCliResourceLimits:
    """Tests for resource limit flags."""

    def test_timeout_flag(self, capsys):
        # A very short timeout should cause a timeout error for slow code
        result = main(["--timeout", "1", "-c", "import time; time.sleep(10)"])
        assert result == 1
        captured = capsys.readouterr()
        assert "timeout" in captured.err.lower()

    def test_timeout_flag_sufficient(self, capsys):
        result = main(["--timeout", "30000", "-c", 'print("ok")'])
        assert result == 0
        captured = capsys.readouterr()
        assert "ok" in captured.out


class TestCliNetworking:
    """Tests for networking flags."""

    def test_net_flag_accepted(self, capsys):
        # Just verify the flag is accepted and code runs
        result = main(["--net", "-c", 'print("net enabled")'])
        assert result == 0
        captured = capsys.readouterr()
        assert "net enabled" in captured.out

    def test_allow_host_implies_net(self, capsys):
        result = main(["--allow-host", "*.example.com", "-c", 'print("ok")'])
        assert result == 0
        captured = capsys.readouterr()
        assert "ok" in captured.out


class TestCliNetConfig:
    """Tests for the NetConfig the CLI builds from networking flags."""

    @staticmethod
    def _net(argv):
        from eryx.__main__ import _build_parser
        from eryx._cli import make_net_config

        return make_net_config(_build_parser().parse_args(argv))

    def test_no_flags_disable_network(self):
        assert self._net([]) is None

    def test_allow_host_is_an_allowlist(self):
        config = self._net(["--allow-host", "api.example.com", "--allow-host", "*.x.com"])
        assert config.allow_all_hosts is False
        assert config.allowed_hosts == ["api.example.com", "*.x.com"]
        assert "localhost" in config.blocked_hosts
        assert "169.254.0.0/16" in config.blocked_networks

    def test_net_keeps_default_blocks(self):
        config = self._net(["--net"])
        assert config.allow_all_hosts is True
        assert "localhost" in config.blocked_hosts
        assert "169.254.0.0/16" in config.blocked_networks


class TestCliResourceLimitsConfig:
    """Tests for the ResourceLimits the CLI builds."""

    @staticmethod
    def _limits(argv):
        from eryx.__main__ import _build_parser
        from eryx._cli import make_resource_limits

        return make_resource_limits(_build_parser().parse_args(argv))

    def test_defaults(self):
        limits = self._limits([])
        assert limits.execution_timeout_ms == 30000
        assert limits.max_memory_bytes == 128 * 1024 * 1024

    def test_flags_override(self):
        limits = self._limits(["--timeout", "5", "--max-memory", "1000"])
        assert limits.execution_timeout_ms == 5
        assert limits.max_memory_bytes == 1000


class TestCliVolume:
    """Tests for volume mount flag."""

    def test_volume_read_file(self, tmp_path, capsys):
        """Test that -v mounts a host directory and reads a file."""
        (tmp_path / "hello.txt").write_text("hello from host")
        result = main(
            ["-v", f"{tmp_path}:/mnt/data", "-c", 'print(open("/mnt/data/hello.txt").read())']
        )
        assert result == 0
        captured = capsys.readouterr()
        assert "hello from host" in captured.out

    def test_volume_read_only(self, tmp_path, capsys):
        """Test that :ro prevents writes."""
        (tmp_path / "existing.txt").write_text("keep me")
        result = main(
            [
                "-v", f"{tmp_path}:/mnt/data:ro",
                "-c", 'open("/mnt/data/new.txt", "w").write("fail")',
            ]
        )
        assert result == 1
        captured = capsys.readouterr()
        # The write should fail with a permission or OS error
        assert "Error" in captured.err or "error" in captured.err

    def test_volume_max_file_size(self, tmp_path):
        """Test that --volume-max-file-size caps files written to a volume."""
        result = main(
            [
                "-v", f"{tmp_path}:/mnt/data",
                "--volume-max-file-size", "4",
                "-c", 'with open("/mnt/data/out.txt", "w") as f: f.write("too long")',
            ]
        )
        assert result == 1
        assert (tmp_path / "out.txt").stat().st_size <= 4

    def test_volume_write_file(self, tmp_path, capsys):
        """Test that a writable volume allows writing back to host."""
        result = main(
            [
                "-v", f"{tmp_path}:/mnt/data",
                "-c", 'open("/mnt/data/output.txt", "w").write("written from sandbox")',
            ]
        )
        assert result == 0
        assert (tmp_path / "output.txt").read_text() == "written from sandbox"

    def test_volume_single_file(self, tmp_path, capsys):
        """Test that -v can mount a single file."""
        host_file = tmp_path / "data.txt"
        host_file.write_text("single file content")
        result = main(
            ["-v", f"{host_file}:/mnt/data.txt:ro", "-c", 'print(open("/mnt/data.txt").read())']
        )
        assert result == 0
        captured = capsys.readouterr()
        assert "single file content" in captured.out

    def test_volume_parse_format_error(self, capsys):
        """Test that an invalid volume spec raises an error."""
        with pytest.raises(SystemExit):
            main(["-v", "invalid_no_colon", "-c", 'print("hi")'])


class TestCliVersion:
    """Tests for --version flag."""

    def test_version_flag(self, capsys):
        with pytest.raises(SystemExit) as exc_info:
            main(["--version"])
        assert exc_info.value.code == 0
        captured = capsys.readouterr()
        assert "eryx" in captured.out


class TestCliMutualExclusion:
    """Tests for mutually exclusive arguments."""

    def test_c_and_script_mutually_exclusive(self, capsys):
        with pytest.raises(SystemExit) as exc_info:
            main(["-c", "print(1)", "script.py"])
        assert exc_info.value.code != 0


class TestCliPipedInput:
    """Tests for piped stdin behavior."""

    def test_piped_stdin_without_dash(self, capsys):
        with patch("sys.stdin") as mock_stdin:
            mock_stdin.isatty.return_value = False
            mock_stdin.read.return_value = 'print("piped")'
            result = main([])
            assert result == 0
            captured = capsys.readouterr()
            assert "piped" in captured.out
