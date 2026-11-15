"""Selecting the guest language of a Sandbox or Session.

The pyeryx wheel only includes the Python runtime. These tests load the
JavaScript and shell runtimes built in the workspace
(`mise run precompile-runtime-javascript` / `-shell`); CI downloads them.
"""

import os
import sys
import types
from pathlib import Path

import pytest

import eryx

RUNTIMES = Path(__file__).parents[2] / "eryx-runtime"


def runtime(language: str) -> str:
    path = RUNTIMES / f"runtime-{language}.cwasm"
    if not path.is_file():
        if os.environ.get("CI"):
            pytest.fail(f"{path} is missing")
        pytest.skip(f"{path} not built")
    return str(path)


@pytest.fixture(autouse=True)
def no_ambient_runtimes(monkeypatch, tmp_path):
    """Each test chooses where the runtime comes from."""
    for language in ("JAVASCRIPT", "SHELL"):
        monkeypatch.delenv(f"ERYX_RUNTIME_CWASM_{language}", raising=False)
        monkeypatch.setitem(sys.modules, f"pyeryx_runtime_{language.lower()}", None)
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path))


def test_python_is_the_default():
    assert eryx.Sandbox().execute("print(6 * 7)").stdout == b"42\n"


def test_explicit_runtime_path():
    sandbox = eryx.Sandbox(language="javascript", runtime_path=runtime("javascript"))
    assert sandbox.execute("console.log(6 * 7)").stdout == b"42\n"


def test_runtime_from_environment(monkeypatch):
    monkeypatch.setenv("ERYX_RUNTIME_CWASM_SHELL", runtime("shell"))
    assert eryx.Sandbox(language="shell").execute("echo $((6 * 7))").stdout == b"42\n"


def test_runtime_from_companion_package(monkeypatch):
    path = runtime("javascript")
    package = types.ModuleType("pyeryx_runtime_javascript")
    package.cwasm_path = lambda: path
    monkeypatch.setitem(sys.modules, "pyeryx_runtime_javascript", package)

    session = eryx.Session(language="javascript")
    session.execute("var x = 6")
    assert session.execute("console.log(x * 7)").stdout == b"42\n"


def test_companion_package_without_its_runtime_is_skipped(monkeypatch):
    def missing():
        raise FileNotFoundError("built without its runtime")

    package = types.ModuleType("pyeryx_runtime_javascript")
    package.cwasm_path = missing
    monkeypatch.setitem(sys.modules, "pyeryx_runtime_javascript", package)
    with pytest.raises(eryx.InitializationError, match=r"pip install pyeryx\[javascript\]"):
        eryx.Sandbox(language="javascript")


def test_missing_runtime_says_how_to_get_it():
    with pytest.raises(eryx.InitializationError, match=r"pip install pyeryx\[javascript\]"):
        eryx.Sandbox(language="javascript")


def test_runtime_for_another_language_is_refused():
    with pytest.raises(eryx.InitializationError, match="runs javascript, but the sandbox is for shell"):
        eryx.Sandbox(language="shell", runtime_path=runtime("javascript"))


def test_unknown_language_is_rejected():
    with pytest.raises(ValueError, match="ruby"):
        eryx.Sandbox(language="ruby")


def test_companion_versions_match_pyeryx():
    """The extras pin the runtime packages to pyeryx's exact version."""
    tomllib = pytest.importorskip("tomllib")  # Python 3.11+
    root = Path(__file__).parents[1]
    version = tomllib.loads((root.parents[1] / "Cargo.toml").read_text())["workspace"]["package"][
        "version"
    ]
    extras = tomllib.loads((root / "pyproject.toml").read_text())["project"]["optional-dependencies"]
    for language in ("javascript", "shell"):
        pin = f"pyeryx-runtime-{language}=={version}"
        assert pin in extras[language] and pin in extras["all"]
        package = tomllib.loads((root / "runtimes" / language / "pyproject.toml").read_text())
        assert package["project"]["version"] == version
