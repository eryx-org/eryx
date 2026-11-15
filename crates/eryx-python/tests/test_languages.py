"""Selecting the guest language of a Sandbox or Session."""

import pytest

import eryx


def test_javascript_sandbox():
    result = eryx.Sandbox(language="javascript").execute("console.log(6 * 7)")
    assert result.stdout == b"42\n"


def test_shell_sandbox():
    result = eryx.Sandbox(language="shell").execute("echo $((6 * 7))")
    assert result.stdout == b"42\n"


def test_python_is_the_default():
    result = eryx.Sandbox().execute("print(6 * 7)")
    assert result.stdout == b"42\n"


def test_session_keeps_javascript_state():
    session = eryx.Session(language="javascript")
    session.execute("var x = 6")
    assert session.execute("console.log(x * 7)").stdout == b"42\n"


def test_unknown_language_is_rejected():
    with pytest.raises(ValueError, match="ruby"):
        eryx.Sandbox(language="ruby")
