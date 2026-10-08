"""Tests for asyncio timers (sleep, wait_for, timeout) inside the sandbox."""

import eryx
import pytest


def test_sleep_and_wait_for():
    result = eryx.Sandbox().execute(
        """
import asyncio
loop = asyncio.get_running_loop()
t0 = loop.time()
await asyncio.sleep(0.05)
print(loop.time() - t0 >= 0.05)
try:
    await asyncio.wait_for(asyncio.sleep(30), timeout=0.05)
except TimeoutError:
    print("timed out")
"""
    )
    assert result.stdout == b"True\ntimed out\n"


def test_execution_timeout_wins_over_sleep():
    limits = eryx.ResourceLimits(execution_timeout_ms=500)
    sandbox = eryx.Sandbox(resource_limits=limits)
    with pytest.raises(eryx.TimeoutError):
        sandbox.execute("import asyncio\nawait asyncio.sleep(60)")
