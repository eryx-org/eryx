"""The client against fakes of the hosted API, as eryx.run's docs describe it.

HTTP goes to an ``httpx.MockTransport``; ``/v1/stream`` to a local
``websockets`` server.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator, Callable
from typing import Any

import httpx
import pytest
import websockets

import eryx_run as eryx

KEY = "eryx_test_key"

SUCCESS = {
    "stdout": "4\n",
    "stderr": "",
    "compute_units": 1234,
    "duration_ms": 12,
    "peak_memory_bytes": 14286848,
    "status": "success",
}

Handler = Callable[[httpx.Request], httpx.Response]


@pytest.fixture
def http(monkeypatch: pytest.MonkeyPatch) -> Callable[[Handler], list[httpx.Request]]:
    """Serve HTTP requests with a handler, returning the requests it got."""

    def install(handler: Handler) -> list[httpx.Request]:
        requests: list[httpx.Request] = []

        def record(request: httpx.Request) -> httpx.Response:
            requests.append(request)
            return handler(request)

        real = httpx.AsyncClient

        def client(**kwargs: Any) -> httpx.AsyncClient:
            return real(transport=httpx.MockTransport(record), **kwargs)

        monkeypatch.setattr(httpx, "AsyncClient", client)
        return requests

    return install


def body(request: httpx.Request) -> Any:
    return json.loads(request.content)


def sandbox(**kwargs: Any) -> eryx.Sandbox:
    return eryx.Sandbox(api_key=KEY, base_url="https://api.test", **kwargs)


# -- Plain runs ---------------------------------------------------------------


async def test_execute_returns_a_result(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    requests = http(lambda _: httpx.Response(200, json={**SUCCESS, "result": {"a": 1}}))
    result = await sandbox().execute_async("print(2 + 2)")

    assert result.stdout == b"4\n" and result.stdout_text == "4\n"
    assert result.fuel_consumed == 1234
    assert result.duration_ms == 12.0
    assert result.peak_memory_bytes == 14286848
    assert result.result == {"a": 1}
    assert isinstance(result, eryx.ExecuteResult)
    [request] = requests
    assert request.url == "https://api.test/v1/execute"
    assert request.headers["authorization"] == f"Bearer {KEY}"
    assert body(request) == {"code": "print(2 + 2)"}


def test_sync_execute_works_inside_an_event_loop(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    http(lambda _: httpx.Response(200, json=SUCCESS))
    assert sandbox().execute("print(2 + 2)").stdout_text == "4\n"

    async def inside() -> str:
        return sandbox().execute("print(2 + 2)").stdout_text

    assert asyncio.run(inside()) == "4\n"


async def test_limits_and_hosted_options_are_sent(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    requests = http(lambda _: httpx.Response(200, json=SUCCESS))
    limits = eryx.ResourceLimits(execution_timeout_ms=1500, max_fuel=10**9)
    await sandbox(
        resource_limits=limits,
        environment="templating",
        connections=["github"],
        network_policies=[],
    ).execute_async("pass")
    assert body(requests[0]) == {
        "code": "pass",
        "timeout_secs": 2,
        "fuel_limit": 10**9,
        "environment": "templating",
        "connections": ["github"],
        "network_policies": [],
    }


def test_local_only_arguments_say_what_to_use() -> None:
    with pytest.raises(ValueError, match="network_policies"):
        sandbox(network=object())
    with pytest.raises(ValueError, match="result"):
        sandbox(result_variable="out")
    with pytest.raises(TypeError, match="nonsense"):
        sandbox(nonsense=1)
    limits = eryx.ResourceLimits(max_memory_bytes=1 << 20)
    with pytest.raises(ValueError, match="max_memory_bytes"):
        sandbox(resource_limits=limits).execute("pass")


def test_a_missing_key_is_explained(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ERYX_API_KEY", raising=False)
    with pytest.raises(ValueError, match="ERYX_API_KEY"):
        eryx.Sandbox()


# -- Failed runs raise what pyeryx raises -------------------------------------

TRACEBACK = "Traceback (most recent call last):\n  ...\nValueError: bad input\n"


@pytest.mark.parametrize(
    ("response", "exception"),
    [
        (
            {"stdout": "loading\n", "stderr": TRACEBACK, "status": "error", "error": TRACEBACK},
            eryx.PythonExceptionError,
        ),
        ({"stdout": "", "stderr": "", "status": "timeout", "error": "timed out"}, eryx.TimeoutError),
        (
            {"stdout": "", "stderr": "", "status": "resource_limit", "error": "Fuel"},
            eryx.ResourceLimitError,
        ),
        (
            {"stdout": "", "stderr": "", "status": "error", "error": "Failed to save"},
            eryx.ExecutionError,
        ),
    ],
)
def test_failed_runs_raise(
    http: Callable[[Handler], list[httpx.Request]],
    response: dict[str, Any],
    exception: type[eryx.EryxError],
) -> None:
    http(lambda _: httpx.Response(200, json={"compute_units": 99, "duration_ms": 5, **response}))
    with pytest.raises(exception) as raised:
        sandbox().execute("...")
    assert type(raised.value) is exception
    assert raised.value.fuel_consumed == 99
    assert raised.value.duration_ms == 5.0
    if isinstance(raised.value, eryx.PythonExceptionError):
        assert raised.value.stdout == b"loading\n"
        assert raised.value.stderr.endswith(b"ValueError: bad input\n")


def test_timeouts_are_builtin_timeouts_too() -> None:
    assert issubclass(eryx.TimeoutError, TimeoutError)
    assert issubclass(eryx.TimeoutError, eryx.EryxError)


@pytest.mark.parametrize(
    ("status", "code", "exception"),
    [
        (401, "invalid_key", eryx.AuthenticationError),
        (404, "not_found", eryx.NotFoundError),
        (409, "session_busy", eryx.ConflictError),
        (429, "concurrency_limit", eryx.RateLimitError),
        (503, "unavailable", eryx.ServiceUnavailableError),
        (418, "something_new", eryx.APIError),
    ],
)
def test_error_responses_are_typed(
    http: Callable[[Handler], list[httpx.Request]],
    status: int,
    code: str,
    exception: type[eryx.APIError],
) -> None:
    http(
        lambda _: httpx.Response(
            status,
            json={"error": {"code": code, "message": "nope"}},
            headers={"Retry-After": "7"},
        )
    )
    with pytest.raises(exception) as raised:
        sandbox().execute("pass")
    assert type(raised.value) is exception
    assert (raised.value.code, raised.value.status_code, raised.value.message) == (
        code,
        status,
        "nope",
    )
    if isinstance(raised.value, eryx.RateLimitError):
        assert raised.value.retry_after == 7.0


# -- Sessions -----------------------------------------------------------------


def test_session_is_created_on_first_run_and_deleted(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/sessions":
            return httpx.Response(200, json={"id": "s1", "environment": "python"})
        if request.method == "DELETE":
            return httpx.Response(204)
        return httpx.Response(200, json=SUCCESS)

    requests = http(handler)
    session = eryx.Session(api_key=KEY, base_url="https://api.test", ttl_secs=60)
    assert session.id is None
    session.execute("x = 1")
    session.execute("print(x)")
    assert session.id == "s1" and session.execution_count == 2
    session.delete()
    assert session.id is None
    assert [(r.method, r.url.path) for r in requests] == [
        ("POST", "/v1/sessions"),
        ("POST", "/v1/sessions/s1/execute"),
        ("POST", "/v1/sessions/s1/execute"),
        ("DELETE", "/v1/sessions/s1"),
    ]
    assert body(requests[0]) == {"ttl_secs": 60}


# -- Callbacks over /v1/stream ------------------------------------------------


@pytest.fixture
async def stream() -> AsyncIterator[dict[str, Any]]:
    """A fake ``/v1/stream`` that calls each listed callback once, concurrently,
    and completes with what they answered."""
    seen: dict[str, Any] = {"messages": []}

    async def serve(ws: websockets.ServerConnection) -> None:
        seen["subprotocol"] = ws.subprotocol
        seen["authorization"] = ws.request.headers["Authorization"] if ws.request else None
        execute = json.loads(await ws.recv())
        seen["messages"].append(execute)
        await ws.send(json.dumps({"type": "something_newer"}))
        for i, callback in enumerate(execute["callbacks"]):
            await ws.send(
                json.dumps(
                    {"type": "callback", "call_id": str(i), "name": callback["name"], "args": {"n": i}}
                )
            )
        answers = {}
        for _ in execute["callbacks"]:
            reply = json.loads(await ws.recv())
            answers[reply["call_id"]] = reply.get("result", reply.get("error"))
        await ws.send(
            json.dumps(
                {
                    "type": "completed",
                    **SUCCESS,
                    "callback_invocations": len(answers),
                    "result": answers,
                    "trace_id": "abc",
                }
            )
        )

    async with websockets.serve(
        serve, "127.0.0.1", 0, subprotocols=[websockets.Subprotocol("eryx.v1")]
    ) as server:
        port = server.sockets[0].getsockname()[1]
        seen["url"] = f"http://127.0.0.1:{port}"
        yield seen


async def test_callbacks_run_over_the_stream(stream: dict[str, Any]) -> None:
    callbacks = eryx.CallbackRegistry()

    @callbacks.callback(description="Doubles n")
    async def double(n: int) -> int:
        return n * 2

    def blocking(n: int) -> str:
        return f"sync {n}"

    def broken(n: int) -> None:
        raise LookupError("no such user")

    callbacks.add(blocking)
    callbacks.add(broken, name="lookup")
    box = eryx.Sandbox(api_key=KEY, base_url=stream["url"], callbacks=callbacks)
    result = await box.execute_async("...")

    assert result.result == {"0": 0, "1": "sync 1", "2": "LookupError: no such user"}
    assert result.callback_invocations == 3
    assert result.trace_id == "abc"
    assert stream["subprotocol"] == "eryx.v1"
    assert stream["authorization"] == f"Bearer {KEY}"
    [execute] = stream["messages"]
    assert execute["callbacks"] == [
        {"name": "double", "description": "Doubles n"},
        {"name": "blocking", "description": ""},
        {"name": "lookup", "description": ""},
    ]


async def test_session_callbacks_send_the_session_id(
    stream: dict[str, Any], http: Callable[[Handler], list[httpx.Request]]
) -> None:
    http(lambda _: httpx.Response(200, json={"id": "s1"}))
    session = eryx.Session(
        api_key=KEY,
        base_url=stream["url"],
        callbacks=[{"name": "f", "fn": lambda n: n}],
    )
    await session.execute_async("x = await f(n=1)")
    assert stream["messages"][0]["session_id"] == "s1"


async def test_stream_errors_raise() -> None:
    async def serve(ws: websockets.ServerConnection) -> None:
        await ws.recv()
        await ws.send(json.dumps({"type": "error", "message": "Too many concurrent executions"}))

    async with websockets.serve(serve, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        box = eryx.Sandbox(
            api_key=KEY,
            base_url=f"http://127.0.0.1:{port}",
            callbacks=[{"name": "f", "fn": lambda: None}],
        )
        with pytest.raises(eryx.APIError, match="Too many"):
            await box.execute_async("...")
