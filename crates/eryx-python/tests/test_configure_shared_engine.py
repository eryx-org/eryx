"""Tests for eryx.configure_shared_engine.

The engine configuration is fixed once per process, so each case runs in a
fresh interpreter.
"""

import os
import subprocess
import sys
import textwrap


def run(code: str, **env: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        capture_output=True,
        text=True,
        env={**os.environ, **env},
        timeout=120,
    )


def test_explicit_settings_beat_the_environment():
    proc = run(
        """
        import eryx
        eryx.configure_shared_engine(allocator="pooling", pool_instances=1, warm_instances=0)
        session = eryx.Session()
        try:
            eryx.Session()
        except eryx.EryxError as e:
            print("LIMIT", e)
        """,
        ERYX_POOL_INSTANCES="50",
    )
    assert proc.returncode == 0, proc.stderr
    assert "concurrent limit of 1" in proc.stdout, proc.stdout


def test_configure_after_first_sandbox_fails():
    proc = run(
        """
        import eryx
        eryx.Sandbox().execute("pass")
        try:
            eryx.configure_shared_engine(warm_instances=0)
        except eryx.InitializationError as e:
            print("FIXED", e)
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "FIXED" in proc.stdout, proc.stdout


def test_invalid_settings_are_rejected():
    proc = run(
        """
        import eryx
        for kwargs in ({"pool_instances": 0}, {"allocator": "mmap"}):
            try:
                eryx.configure_shared_engine(**kwargs)
            except (ValueError, eryx.InitializationError) as e:
                print("REJECTED", type(e).__name__)
        # Allocator names are case-insensitive, like ERYX_ALLOCATOR.
        eryx.configure_shared_engine(allocator="On-Demand", warm_instances=0)
        print("OK")
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.split() == [
        "REJECTED",
        "InitializationError",
        "REJECTED",
        "ValueError",
        "OK",
    ], proc.stdout
