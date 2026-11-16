"""External commands for shell sandboxes (`commands=`, `eryx.Command`), the
coreutils that `pyeryx-runtime-shell` brings, and the shell's exit code.

Uses the shell runtime and coreutils built in the workspace
(`mise run precompile-runtime-shell` / `precompile-coreutils`); CI downloads them.
"""

import os
import sys
import types
from pathlib import Path

import pytest

import eryx

RUNTIMES = Path(__file__).parents[2] / "eryx-runtime"


def built(name: str) -> str:
    path = RUNTIMES / name
    if not path.is_file():
        if os.environ.get("CI"):
            pytest.fail(f"{path} is missing")
        pytest.skip(f"{path} not built")
    return str(path)


@pytest.fixture(autouse=True)
def shell_runtime(monkeypatch):
    """The workspace's shell runtime, and no companion package unless a test
    installs one."""
    monkeypatch.setenv("ERYX_RUNTIME_CWASM_SHELL", built("runtime-shell.cwasm"))
    monkeypatch.setitem(sys.modules, "pyeryx_runtime_shell", None)


def companion(monkeypatch, coreutils=True):
    package = types.ModuleType("pyeryx_runtime_shell")
    if coreutils:
        package.coreutils_path = lambda: built("coreutils.cwasm")
    monkeypatch.setitem(sys.modules, "pyeryx_runtime_shell", package)


def test_without_the_companion_the_shell_has_no_commands():
    with pytest.raises(eryx.PythonExceptionError) as raised:
        eryx.Sandbox(language="shell").execute("echo hi | sort")
    assert raised.value.exit_code == 127


def test_the_companions_coreutils_are_the_default_commands(monkeypatch):
    companion(monkeypatch)
    result = eryx.Sandbox(language="shell").execute(
        "printf 'pear\\napple\\n' | sort | head -n 1"
    )
    assert result.stdout == b"apple\n"


def test_a_companion_without_coreutils_is_fine(monkeypatch):
    companion(monkeypatch, coreutils=False)
    assert eryx.Sandbox(language="shell").execute("echo hi").stdout == b"hi\n"


def test_commands_by_name_replace_coreutils_tools(monkeypatch):
    companion(monkeypatch)
    # A multicall binary picks the tool from argv[0], so a registered name
    # must be one of its tools'; this one replaces the companion's `head`.
    coreutils = eryx.Command.from_precompiled_file(built("coreutils.cwasm"))
    sandbox = eryx.Sandbox(language="shell", commands={"head": coreutils})
    assert sandbox.execute("printf 'b\\na\\n' | sort | head -n 1").stdout == b"a\n"

    session = eryx.Session(language="shell", commands={"head": coreutils})
    session.execute("x=$(printf 'z\\ny\\n' | head -n 1)")
    assert session.execute('echo "$x"').stdout == b"z\n"


def test_exit_code_is_on_the_error():
    sandbox = eryx.Sandbox(language="shell")
    with pytest.raises(eryx.PythonExceptionError) as raised:
        sandbox.execute("echo before; exit 3")
    assert raised.value.exit_code == 3
    assert raised.value.stdout == b"before\n"


def test_python_errors_have_no_exit_code():
    with pytest.raises(eryx.PythonExceptionError) as raised:
        eryx.Sandbox().execute("raise ValueError('x')")
    assert raised.value.exit_code is None


def test_commands_are_only_for_the_shell():
    command = eryx.Command.from_precompiled_file(built("coreutils.cwasm"))
    with pytest.raises(ValueError, match='only for language="shell"'):
        eryx.Sandbox(commands={"cat": command})


def test_a_bad_command_fails_clearly(tmp_path):
    with pytest.raises(eryx.InitializationError):
        eryx.Command.from_file(tmp_path / "missing.wasm")
    with pytest.raises(eryx.InitializationError):
        eryx.Command.from_bytes(b"not wasm")


def setup_cache(monkeypatch, tmp_path, stems):
    """An `eryx-precompile setup` cache holding the workspace's builds."""
    import platform
    import shutil

    cache = tmp_path / "eryx"
    cache.mkdir(mode=0o700)
    arch = {"x86_64": "x86_64", "AMD64": "x86_64", "arm64": "aarch64"}.get(
        platform.machine(), platform.machine()
    )
    target = f"{arch}-{sys.platform.replace('darwin', 'macos').replace('win32', 'windows')}"
    for stem in stems:
        path = cache / f"{stem}-v{eryx.__version__}-{target}.cwasm"
        shutil.copy(built(f"{stem}.cwasm"), path)
        path.chmod(0o600)
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path))
    monkeypatch.delenv("ERYX_RUNTIME_CWASM_SHELL")


@pytest.mark.skipif(sys.platform == "win32", reason="the cache's private-file checks are Unix")
def test_a_setup_cache_runtime_gets_the_caches_coreutils(monkeypatch, tmp_path):
    setup_cache(monkeypatch, tmp_path, ["runtime-shell", "coreutils"])
    result = eryx.Sandbox(language="shell").execute("printf 'b\\na\\n' | sort")
    assert result.stdout == b"a\nb\n"


def test_another_sources_broken_coreutils_is_ignored(monkeypatch, tmp_path):
    broken = tmp_path / "coreutils.cwasm"
    broken.write_bytes(b"not a precompiled command")
    package = types.ModuleType("pyeryx_runtime_shell")
    package.coreutils_path = lambda: str(broken)
    monkeypatch.setitem(sys.modules, "pyeryx_runtime_shell", package)

    # The runtime came from the environment, so the companion's coreutils
    # aren't its own: one that doesn't load counts as none.
    assert eryx.Sandbox(language="shell").execute("echo hi").stdout == b"hi\n"

    # The companion's own runtime with a broken coreutils names the file.
    monkeypatch.delenv("ERYX_RUNTIME_CWASM_SHELL")
    runtime = built("runtime-shell.cwasm")
    package.cwasm_path = lambda: runtime
    with pytest.raises(eryx.InitializationError, match="coreutils.cwasm"):
        eryx.Sandbox(language="shell")


CA = "/etc/ssl/certs/ca-certificates.crt"


@pytest.mark.parametrize(
    "child_network", ["inherit", eryx.NetConfig(allowed_hosts=["api.test"])]
)
def test_child_network_gives_commands_the_proxy(child_network):
    # A child with network gets the proxy's CA certificate mounted.
    coreutils = eryx.Command.from_precompiled_file(
        built("coreutils.cwasm"), allow_tunnels=True
    )
    network = eryx.NetConfig(allowed_hosts=["api.test"])
    sandbox = eryx.Sandbox(
        language="shell",
        commands={"cat": coreutils},
        network=network,
        child_network=child_network,
    )
    assert b"BEGIN CERTIFICATE" in sandbox.execute(f"cat {CA}").stdout

    session = eryx.Session(
        language="shell",
        commands={"cat": coreutils},
        network=network,
        child_network=child_network,
    )
    assert b"BEGIN CERTIFICATE" in session.execute(f"cat {CA}").stdout

    # Without it, children stay offline.
    offline = eryx.Sandbox(
        language="shell", commands={"cat": coreutils}, network=network
    )
    with pytest.raises(eryx.PythonExceptionError):
        offline.execute(f"cat {CA}")


def test_child_network_is_checked():
    network = eryx.NetConfig(allowed_hosts=["api.test"])
    with pytest.raises(ValueError, match="needs network="):
        eryx.Sandbox(language="shell", child_network="inherit")
    with pytest.raises(ValueError, match='only for language="shell"'):
        eryx.Sandbox(network=network, child_network="inherit")
    with pytest.raises(ValueError, match='not "all"'):
        eryx.Sandbox(language="shell", network=network, child_network="all")
    with pytest.raises(TypeError):
        eryx.Session(language="shell", network=network, child_network=1)
