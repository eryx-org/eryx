"""Regression tests for factory-backed persistent sessions."""

import gc
import threading
import time
import zipfile

import eryx
import pytest


def test_factory_session_persistent_state_and_full_reset(sandbox_factory):
    session = sandbox_factory.create_session()
    assert session.execution_timeout_ms is None
    assert session.fuel_limit is None
    session.execute("value = 41")
    assert session.execute("print(value + 1)").stdout == "42"
    session.reset()
    with pytest.raises(eryx.ExecutionError):
        session.execute("print(value)")


def test_factory_session_result_and_reset(sandbox_factory):
    session = sandbox_factory.create_session(result_variable="answer")
    assert session.execute("answer = {'ok': True}").result == {"ok": True}
    session.reset()
    assert session.execute("answer = 7").result == 7


def test_factory_session_memory_growth_rejected(sandbox_factory):
    with pytest.raises(eryx.InitializationError):
        sandbox_factory.create_session(
            resource_limits=eryx.ResourceLimits(max_memory_bytes=1)
        )
    session = sandbox_factory.create_session(
        resource_limits=eryx.ResourceLimits(max_memory_bytes=128 * 1024 * 1024)
    )
    session.execute("x = bytearray(1024)")
    with pytest.raises((eryx.ExecutionError, eryx.ResourceLimitError)):
        session.execute("x = bytearray(512 * 1024 * 1024)")
    session.reset()
    with pytest.raises((eryx.ExecutionError, eryx.ResourceLimitError)):
        session.execute("x = bytearray(512 * 1024 * 1024)")


def test_factory_session_timeout_and_fuel_limits(sandbox_factory):
    timeout = sandbox_factory.create_session(
        resource_limits=eryx.ResourceLimits(execution_timeout_ms=100)
    )
    with pytest.raises(eryx.TimeoutError):
        timeout.execute("while True: pass")

    fuel = sandbox_factory.create_session(
        resource_limits=eryx.ResourceLimits(max_fuel=100)
    )
    with pytest.raises(eryx.ResourceLimitError):
        fuel.execute("while True: pass")
    fuel.reset()
    with pytest.raises(eryx.ResourceLimitError):
        fuel.execute("while True: pass")


def test_ordinary_session_defaults_remain_unchanged():
    session = eryx.Session()
    assert session.execution_timeout_ms is None
    assert session.vfs is None

    # vfs_mount_path relocates caller-supplied storage; alone it must not
    # materialise a VFS, so a storage-less session still reports none.
    bare = eryx.Session(vfs_mount_path="/custom")
    assert bare.vfs is None
    assert bare.vfs_mount_path is None


def test_factory_session_callback_count_applies_after_reset(sandbox_factory):
    calls = []

    def callback():
        calls.append(1)
        return len(calls)

    session = sandbox_factory.create_session(
        callbacks=[{"name": "count", "fn": callback}],
        resource_limits=eryx.ResourceLimits(max_callback_invocations=1),
    )
    # max_callback_invocations is per execution, so both single-callback calls succeed.
    assert session.execute("print(await count())").stdout == "1"
    assert session.execute("print(await count())").stdout == "2"
    session.reset()
    with pytest.raises(eryx.ExecutionError):
        session.execute("await count(); await count()")
    assert len(calls) == 3


def test_factory_session_callback_timeout(sandbox_factory):
    started = threading.Event()
    release = threading.Event()
    finished = threading.Event()

    def slow_callback():
        started.set()
        try:
            release.wait(timeout=5)
            return 1
        finally:
            finished.set()

    session = sandbox_factory.create_session(
        callbacks=[{"name": "slow", "fn": slow_callback}],
        resource_limits=eryx.ResourceLimits(callback_timeout_ms=50),
    )
    started_at = time.monotonic()
    try:
        with pytest.raises(eryx.ExecutionError, match="timeout"):
            session.execute("await slow()")
        assert started.is_set()
        assert time.monotonic() - started_at < 1.0
    finally:
        # spawn_blocking work continues after the host timeout.
        release.set()
        assert finished.wait(timeout=5)


def test_factory_session_caller_vfs_keeps_own_policy_and_mount_path(sandbox_factory):
    storage = eryx.VfsStorage()
    session = sandbox_factory.create_session(
        vfs=storage,
        vfs_mount_path="/custom",
        resource_limits=eryx.ResourceLimits(max_vfs_bytes=1),
    )
    session.execute("with open('/custom/a', 'w') as f: f.write('caller-owned')")
    session.reset()
    assert session.execute("print(open('/custom/a').read())").stdout == "caller-owned"
    assert session.vfs_mount_path == "/custom"


def test_factory_session_volumes_output_and_native_import(
    sandbox_factory, markupsafe_wheel, tmp_path
):
    volume = tmp_path / "volume"
    volume.mkdir()
    output = []
    errors = []
    session = sandbox_factory.create_session(
        volumes=[(str(volume), "/mnt", True)],
        on_stdout=output.append,
        on_stderr=errors.append,
    )
    assert session.execute("import os; print(os.path.isdir('/mnt'))").stdout == "True"
    assert output
    with pytest.raises(eryx.ExecutionError):
        session.execute("with open('/mnt/nope', 'w') as f: f.write('x')")
    session.execute("import sys; print('diagnostic', file=sys.stderr)")
    assert errors

    factory = eryx.SandboxFactory(packages=[str(markupsafe_wheel)], imports=[])
    imported = factory.create_session(on_stdout=output.append)
    del factory
    gc.collect()
    assert (
        imported.execute(
            "import markupsafe; import markupsafe._speedups; "
            "print(markupsafe._speedups.__name__)"
        ).stdout
        == "markupsafe._speedups"
    )


def test_factory_session_network(http_server, sandbox_factory):
    host, port = http_server
    net = eryx.NetConfig.permissive().allow_localhost()
    session = sandbox_factory.create_session(network=net)
    result = session.execute(f"""
import socket

sock = socket.create_connection(("{host}", {port}), timeout=5)
sock.sendall(b"GET / HTTP/1.1\\r\\nHost: {host}:{port}\\r\\nConnection: close\\r\\n\\r\\n")
chunks = []
while True:
    chunk = sock.recv(4096)
    if not chunk:
        break
    chunks.append(chunk)
sock.close()
print(b"".join(chunks).decode(errors="replace"))
""")
    assert "Hello from test server" in result.stdout


def test_factory_session_preimport_and_local_wheel_lifetime(sandbox_factory, tmp_path):
    wheel = tmp_path / "tiny-1.0-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("tiny_late.py", "VALUE = 42\n")
        archive.writestr(
            "tiny-1.0.dist-info/METADATA",
            "Metadata-Version: 2.1\nName: tiny\nVersion: 1.0\n",
        )
        archive.writestr("tiny-1.0.dist-info/WHEEL", "Wheel-Version: 1.0\n")
    factory = eryx.SandboxFactory(packages=[str(wheel)], imports=["json"])
    session = factory.create_session()
    assert session.execute("import sys; print('json' in sys.modules)").stdout == "True"
    del factory
    gc.collect()
    assert session.execute("import tiny_late; print(tiny_late.VALUE)").stdout == "42"
    session.reset()
    assert session.execute("import tiny_late; print(tiny_late.VALUE)").stdout == "42"


def test_saved_factory_session_lazily_imports_from_supplied_site_packages(tmp_path):
    site_packages = tmp_path / "site-packages"
    package = site_packages / "saved_factory_package"
    package.mkdir(parents=True)
    (package / "__init__.py").write_text("VALUE = 42\n")

    save_path = tmp_path / "factory.bin"
    factory = eryx.SandboxFactory(site_packages=site_packages, imports=[])
    factory.save(save_path)

    loaded = eryx.SandboxFactory.load(save_path, site_packages=site_packages)
    session = loaded.create_session()
    assert (
        session.execute(
            "import sys; print('saved_factory_package' not in sys.modules)"
        ).stdout
        == "True"
    )
    assert (
        session.execute(
            "import saved_factory_package; print(saved_factory_package.VALUE)"
        ).stdout
        == "42"
    )


def test_cached_loaded_factory_sessions_are_isolated_and_equivalent(
    sandbox_factory, tmp_path
):
    save_path = tmp_path / "cached-factory.bin"
    sandbox_factory.save(save_path)
    loaded = eryx.SandboxFactory.load(save_path, cache=True)

    first = loaded.create_session()
    second = loaded.create_session()
    assert first.execute("print('clean' if 'state' not in globals() else 'dirty')").stdout == (
        "clean"
    )
    assert second.execute("print('clean' if 'state' not in globals() else 'dirty')").stdout == (
        "clean"
    )

    first.execute("state = 'first'")
    second.execute("state = 'second'")
    assert first.execute("print(state)").stdout == "first"
    assert second.execute("print(state)").stdout == "second"
