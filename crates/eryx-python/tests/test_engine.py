"""Tests for eryx.Engine.

An Engine ignores the ERYX_* environment variables, so the cases that set them
run in a fresh interpreter. The environment there is invalid for the shared
engine, so any code path that fell back to it would fail.
"""

import os
import subprocess
import sys
import textwrap

import eryx
import pytest

INVALID_SHARED_ENV = {
    "ERYX_POOL_INSTANCES": "50",
    "ERYX_POOL_KEEP_RESIDENT_MB": "lots",
}


def run(code: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        capture_output=True,
        text=True,
        env={**os.environ, **INVALID_SHARED_ENV},
        timeout=300,
    )


def test_invalid_settings_are_rejected():
    with pytest.raises(eryx.InitializationError):
        eryx.Engine(pool_instances=0)
    with pytest.raises(ValueError):
        eryx.Engine(allocator="mmap")


def test_engine_ignores_the_environment_and_applies_its_own_limits():
    proc = run(
        """
        import eryx

        engine = eryx.Engine(allocator="pooling", pool_instances=1, warm_instances=0)
        print(eryx.Sandbox(engine=engine).execute("print(6 * 7)").stdout_text.strip())
        session = eryx.Session(engine=engine)
        try:
            eryx.Session(engine=engine)
        except eryx.EryxError as e:
            print("LIMIT", e)
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.startswith("42\n"), proc.stdout
    assert "concurrent limit of 1" in proc.stdout, proc.stdout


def test_factory_sandboxes_sessions_and_pools_use_its_engine(tmp_path):
    path = tmp_path / "factory.bin"
    proc = run(
        f"""
        import eryx

        engine = eryx.Engine(allocator="on-demand", warm_instances=0)
        factory = eryx.SandboxFactory(engine=engine)
        print(factory.create_sandbox().execute("print(1)").stdout_text.strip())
        factory.save({str(path)!r})

        small = eryx.Engine(allocator="pooling", pool_instances=1, warm_instances=0)
        loaded = eryx.SandboxFactory.load({str(path)!r}, engine=small)
        print(loaded.create_sandbox().execute("print(2)").stdout_text.strip())
        with loaded.create_pool(max_size=1, min_idle=0) as pool:
            with pool.acquire() as sandbox:
                print(sandbox.execute("print(3)").stdout_text.strip())
        session = loaded.create_session()
        try:
            loaded.create_session()
        except eryx.EryxError as e:
            print("LIMIT", e)
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.startswith("1\n2\n3\n"), proc.stdout
    assert "concurrent limit of 1" in proc.stdout, proc.stdout
