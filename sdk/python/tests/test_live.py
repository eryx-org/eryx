"""The client against a real eryx.run.

Skipped unless ``ERYX_LIVE_TESTS=1``, with ``ERYX_API_KEY`` and ``ERYX_URL``
set (e.g. a local eryx-run server and its seed key). Sessions need the server
to have a database.
"""

from __future__ import annotations

import os

import pytest

import eryx_run as eryx

pytestmark = pytest.mark.skipif(
    os.environ.get("ERYX_LIVE_TESTS") != "1", reason="set ERYX_LIVE_TESTS=1 to run"
)


def test_execute() -> None:
    result = eryx.Sandbox().execute("import math\nprint(math.factorial(5))\nresult = [1, 2]")
    assert result.stdout_text == "120\n"
    assert result.result == [1, 2]
    assert result.fuel_consumed and result.fuel_consumed > 0
    assert result.duration_ms >= 0


def test_failures_raise_what_pyeryx_raises() -> None:
    sandbox = eryx.Sandbox()
    with pytest.raises(eryx.PythonExceptionError) as raised:
        sandbox.execute("print('before')\nraise ValueError('bad input')")
    assert raised.value.stdout == b"before\n"
    assert b"ValueError: bad input" in raised.value.stderr

    limits = eryx.ResourceLimits(max_fuel=1_000_000)
    with pytest.raises(eryx.ResourceLimitError):
        eryx.Sandbox(resource_limits=limits).execute("while True: pass")


def test_bad_key() -> None:
    with pytest.raises(eryx.AuthenticationError):
        eryx.Sandbox(api_key="eryx_dev_wrong").execute("pass")


async def test_callbacks() -> None:
    calls = []

    async def get_user(id: int) -> dict[str, object]:
        calls.append(id)
        return {"id": id, "name": "Ada"}

    def fails() -> None:
        raise LookupError("nope")

    sandbox = eryx.Sandbox(
        callbacks=[
            {"name": "get_user", "fn": get_user},
            {"name": "fails", "fn": fails},
        ]
    )
    result = await sandbox.execute_async(
        """
import asyncio
users = await asyncio.gather(get_user(id=1), get_user(id=2))
try:
    await fails()
except RuntimeError as e:
    print("raised", e)
result = [u["name"] for u in users]
"""
    )
    assert sorted(calls) == [1, 2]
    assert result.result == ["Ada", "Ada"]
    assert result.callback_invocations == 3
    assert "raised" in result.stdout_text and "nope" in result.stdout_text


def test_session() -> None:
    session = eryx.Session()
    session.execute("x = 21")
    assert session.execute("print(x * 2)").stdout_text == "42\n"
    session_id = session.id
    assert session_id is not None

    # The same session over the stream, from another object
    again = eryx.Session(id=session_id, callbacks=[{"name": "double", "fn": lambda n: n * 2}])
    assert again.execute("result = await double(n=x)").result == 42

    session.delete()
    with pytest.raises(eryx.NotFoundError):
        eryx.Session(id=session_id).execute("pass")


def test_stream_speaks_v1_and_types_errors() -> None:
    # An unknown environment, over the stream: a typed error with its code
    sandbox = eryx.Sandbox(environment="no-such-env", callbacks=[{"name": "f", "fn": lambda: 1}])
    with pytest.raises(eryx.NotFoundError) as raised:
        sandbox.execute("await f()")
    assert raised.value.code == "not_found"


def test_big_integer_results_are_exact() -> None:
    assert eryx.Sandbox().execute("result = 2**100").result == 2**100
