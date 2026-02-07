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


class TestCliVolume:
    """Tests for volume mount flag."""

    def test_volume_flag_not_implemented(self, capsys):
        result = main(["-v", "/tmp:/data", "-c", 'print("hi")'])
        assert result == 1
        captured = capsys.readouterr()
        assert "not yet implemented" in captured.err


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
