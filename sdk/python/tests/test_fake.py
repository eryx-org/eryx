"""The client against fakes of the hosted API, as eryx.run's docs describe it.

HTTP goes to an ``httpx.MockTransport``; ``/v1/stream`` to a local
``websockets`` server.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator, Callable
from pathlib import Path
from typing import Any

import httpx
import pytest
import websockets

import eryx_run as eryx
from eryx_run._mounts import answer, mounts

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


async def test_stream_error_codes_are_typed() -> None:
    async def serve(ws: websockets.ServerConnection) -> None:
        await ws.recv()
        await ws.send(
            json.dumps({"type": "error", "code": "concurrency_limit", "message": "Too many"})
        )

    async with websockets.serve(serve, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        box = eryx.Sandbox(
            api_key=KEY,
            base_url=f"http://127.0.0.1:{port}",
            callbacks=[{"name": "f", "fn": lambda: None}],
        )
        with pytest.raises(eryx.RateLimitError) as raised:
            await box.execute_async("...")
    assert raised.value.code == "concurrency_limit"
    assert raised.value.status_code is None


# -- Review regressions --------------------------------------------------------


async def test_a_result_json_cant_hold_is_an_error_reply(stream: dict[str, Any]) -> None:
    box = eryx.Sandbox(
        api_key=KEY, base_url=stream["url"], callbacks=[{"name": "f", "fn": lambda n: {1, 2}}]
    )
    result = await box.execute_async("...")
    [answer] = result.result.values()
    assert answer.startswith("TypeError:"), answer


def test_an_unreachable_server_is_an_eryx_error() -> None:
    box = eryx.Sandbox(api_key=KEY, base_url="http://127.0.0.1:9")
    with pytest.raises(eryx.APIConnectionError):
        box.execute("pass")
    streaming = eryx.Sandbox(
        api_key=KEY, base_url="http://127.0.0.1:9", callbacks=[{"name": "f", "fn": lambda: 1}]
    )
    with pytest.raises(eryx.APIConnectionError):
        streaming.execute("pass")


async def test_a_dropped_stream_is_an_eryx_error() -> None:
    async def serve(ws: websockets.ServerConnection) -> None:
        await ws.recv()
        await ws.close(code=1011)

    async with websockets.serve(serve, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        box = eryx.Sandbox(
            api_key=KEY, base_url=f"http://127.0.0.1:{port}", callbacks=[{"name": "f", "fn": lambda: 1}]
        )
        with pytest.raises(eryx.APIConnectionError):
            await box.execute_async("...")


def test_a_printed_traceback_isnt_a_raised_exception(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    http(
        lambda _: httpx.Response(
            200,
            json={"stdout": "", "stderr": TRACEBACK, "status": "error", "error": "Failed to save session state"},
        )
    )
    with pytest.raises(eryx.ExecutionError) as raised:
        sandbox().execute("...")
    assert type(raised.value) is eryx.ExecutionError


async def test_concurrent_first_runs_dont_create_two_sessions(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/sessions":
            return httpx.Response(200, json={"id": "s1"})
        return httpx.Response(200, json=SUCCESS)

    requests = http(handler)
    session = eryx.Session(api_key=KEY, base_url="https://api.test")
    real_request = session._connection.request

    async def delayed(method: str, path: str, body: Any = None) -> Any:
        if path == "/v1/sessions":
            await asyncio.sleep(0.05)
        return await real_request(method, path, body)

    session._connection.request = delayed  # type: ignore[method-assign]
    results = await asyncio.gather(
        session.execute_async("a"), session.execute_async("b"), return_exceptions=True
    )
    assert sum(isinstance(r, eryx.ConflictError) for r in results) == 1
    assert [r.url.path for r in requests].count("/v1/sessions") == 1
    assert session.execution_count == 1


def test_session_ids_are_escaped(http: Callable[[Handler], list[httpx.Request]]) -> None:
    requests = http(lambda _: httpx.Response(200, json=SUCCESS))
    eryx.Session(api_key=KEY, base_url="https://api.test", id="a/../b").execute("pass")
    assert requests[0].url.raw_path == b"/v1/sessions/a%2F..%2Fb/execute"


# -- Connection reuse ----------------------------------------------------------


def test_http_runs_share_one_client(
    monkeypatch: pytest.MonkeyPatch, http: Callable[[Handler], list[httpx.Request]]
) -> None:
    http(lambda _: httpx.Response(200, json=SUCCESS))
    made = []
    patched = httpx.AsyncClient

    def counting(**kwargs: Any) -> httpx.AsyncClient:
        made.append(1)
        return patched(**kwargs)

    monkeypatch.setattr(httpx, "AsyncClient", counting)
    box = sandbox()
    for _ in range(3):
        box.execute("pass")
    assert len(made) == 1
    box.close()
    box.execute("pass")
    assert len(made) == 2


@pytest.fixture
async def multi_stream() -> AsyncIterator[dict[str, Any]]:
    """A fake ``/v1/stream`` that runs any number of executes per connection
    (calling each listed callback once), refuses code ``"refuse"``, and counts
    connections."""
    seen: dict[str, Any] = {"connections": 0}

    async def serve(ws: websockets.ServerConnection) -> None:
        seen["connections"] += 1
        async for raw in ws:
            execute = json.loads(raw)
            if execute.get("code") == "refuse":
                await ws.send(json.dumps({"type": "error", "code": "session_busy", "message": "busy"}))
                continue
            for i, callback in enumerate(execute["callbacks"]):
                await ws.send(
                    json.dumps({"type": "callback", "call_id": str(i), "name": callback["name"], "args": {}})
                )
                await ws.recv()
            await ws.send(json.dumps({"type": "completed", **SUCCESS}))

    async with websockets.serve(serve, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        seen["url"] = f"http://127.0.0.1:{port}"
        yield seen


def streaming(url: str) -> eryx.Sandbox:
    return eryx.Sandbox(api_key=KEY, base_url=url, callbacks=[{"name": "f", "fn": lambda: 1}])


async def test_stream_runs_reuse_a_connection(multi_stream: dict[str, Any]) -> None:
    box = streaming(multi_stream["url"])
    for _ in range(3):
        await box.execute_async("...")
    with pytest.raises(eryx.ConflictError):
        await box.execute_async("refuse")
    await box.execute_async("...")
    assert multi_stream["connections"] == 1

    # Concurrent runs need a connection each, and both are kept
    await asyncio.gather(box.execute_async("..."), box.execute_async("..."))
    assert multi_stream["connections"] == 2
    await asyncio.gather(box.execute_async("..."), box.execute_async("..."))
    assert multi_stream["connections"] == 2

    await box.aclose()
    await box.execute_async("...")
    assert multi_stream["connections"] == 3


async def test_idle_stream_connections_expire(
    multi_stream: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(eryx._client, "IDLE_STREAM_SECS", 0.0)
    box = streaming(multi_stream["url"])
    await box.execute_async("...")
    await box.execute_async("...")
    assert multi_stream["connections"] == 2


async def test_sync_stream_runs_reuse_a_connection(multi_stream: dict[str, Any]) -> None:
    # In a thread, so this loop keeps serving the fake
    def run() -> None:
        with streaming(multi_stream["url"]) as box:
            box.execute("...")
            box.execute("...")

    await asyncio.to_thread(run)
    assert multi_stream["connections"] == 1


async def test_sync_calls_from_async_callbacks_are_refused(
    http: Callable[[Handler], list[httpx.Request]],
) -> None:
    http(lambda _: httpx.Response(200, json=SUCCESS))
    inner = sandbox()

    async def call_sync() -> str:
        return inner.execute("pass").stdout_text

    # The callback runs on the sync loop when the outer run is sync
    with pytest.raises(RuntimeError, match="_async"):
        eryx._sandbox._run_sync(call_sync())


# -- Volumes --------------------------------------------------------------------



@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A directory to mount, with secrets beside it and in it."""
    root = tmp_path / "repo"
    (root / "src").mkdir(parents=True)
    (root / "README.md").write_text("# hello\n")
    (root / "src" / "big.bin").write_bytes(bytes(100_000))
    (root / ".env").write_text("TOKEN=not-a-real-token\n")
    (root / ".git").mkdir()
    (root / ".git" / "config").write_text("[remote]\n")
    (root / ".git" / "HEAD").write_text("ref: refs/heads/main\n")
    (tmp_path / "outside.txt").write_text("outside\n")
    (root / "out").symlink_to(tmp_path / "outside.txt")
    (root / "env-link").symlink_to(root / ".env")
    (root / "readme-link").symlink_to(root / "README.md")
    return root


def fs(served: Any, op: str, path: str, **extra: Any) -> tuple[dict[str, Any], bytes]:
    message = {"type": "fs", "op_id": "1", "op": op, "mount": "/mnt/repo", "path": path, **extra}
    return answer(served, message)


def test_volumes_serve_files_and_listings(repo: Path) -> None:
    served = mounts([(str(repo), "/mnt/repo", True)], eryx.DEFAULT_HIDDEN)

    reply, data = fs(served, "list", "")
    names = [e["name"] for e in reply["entries"]]
    assert names == [".git", "README.md", "readme-link", "src"]
    readme = next(e for e in reply["entries"] if e["name"] == "README.md")
    assert readme["inline"] and readme["size"] == 8 and not readme["is_dir"]
    # Inline contents come in the entries' order
    assert data == b"# hello\n# hello\n" and reply["data"] == len(data)

    reply, _ = fs(served, "stat", "src/big.bin")
    assert reply["stat"]["size"] == 100_000
    reply, data = fs(served, "read", "src/big.bin", offset=99_990, len=100)
    assert data == bytes(10) and reply["data"] == 10
    reply, _ = fs(served, "list", "src")
    [big] = reply["entries"]
    assert "inline" not in big
    assert fs(served, "list", ".git")[0]["entries"][0]["name"] == "HEAD"
    assert fs(served, "list", "README.md")[0]["error"] == "not_a_directory"
    assert fs(served, "stat", "missing")[0]["error"] == "not_found"


def test_volumes_refuse_what_they_mustnt_serve(repo: Path) -> None:
    served = mounts([(str(repo), "/mnt/repo", True)], eryx.DEFAULT_HIDDEN)
    for path in ["..", "../outside.txt", "src/../../outside.txt", "/etc/passwd", "./README.md", "src//big.bin"]:
        assert fs(served, "stat", path)[0]["error"] == "permission_denied", path
    # Out of the mount, hidden, or a link to a hidden file: as if missing
    for path in ["out", ".env", ".git/config", "env-link"]:
        reply, data = fs(served, "read", path, offset=0, len=100)
        assert reply["error"] == "not_found" and data == b"", path
    assert answer(served, {"op_id": "1", "op": "stat", "mount": "/mnt/other", "path": ""})[0][
        "error"
    ] == "not_found"

    everything = mounts([(str(repo), "/mnt/repo", True)], ())
    assert fs(everything, "read", ".env", offset=0, len=5)[1] == b"TOKEN"
    assert fs(everything, "stat", "out")[0]["error"] == "not_found"


def test_volumes_are_read_only_under_mnt(repo: Path) -> None:
    with pytest.raises(ValueError, match="read-only"):
        sandbox(volumes=[(str(repo), "/mnt/repo", False)])
    with pytest.raises(ValueError, match="/mnt/"):
        sandbox(volumes=[(str(repo), "/data/repo", True)])
    with pytest.raises(ValueError, match="directory"):
        sandbox(volumes=[(str(repo / "README.md"), "/mnt/readme", True)])


async def test_volumes_are_served_over_the_stream(repo: Path) -> None:
    seen: dict[str, Any] = {}

    async def serve(ws: websockets.ServerConnection) -> None:
        seen["execute"] = json.loads(await ws.recv())
        for i, (op, path) in enumerate([("list", ""), ("read", "src/big.bin")]):
            message = {"type": "fs", "op_id": str(i), "op": op, "mount": "/mnt/repo", "path": path}
            await ws.send(json.dumps({**message, "offset": 0, "len": 4}))
        replies = {}
        for _ in range(2):
            reply = json.loads(await ws.recv())
            replies[reply["op_id"]] = (reply, await ws.recv() if reply.get("data") else b"")
        seen["replies"] = replies
        await ws.send(json.dumps({"type": "completed", **SUCCESS}))

    async with websockets.serve(serve, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        box = eryx.Sandbox(
            api_key=KEY,
            base_url=f"http://127.0.0.1:{port}",
            volumes=[(str(repo), "/mnt/repo", True)],
        )
        await box.execute_async("...")

    assert seen["execute"]["mounts"] == [{"path": "/mnt/repo"}]
    listing, inline = seen["replies"]["0"]
    assert [e["name"] for e in listing["entries"]][:2] == [".git", "README.md"]
    assert inline == b"# hello\n# hello\n"
    assert seen["replies"]["1"] == ({"type": "fs_response", "op_id": "1", "data": 4}, bytes(4))


def test_volumes_answer_bad_messages_and_bound_reads(repo: Path) -> None:
    served = mounts([(str(repo), "/mnt/repo", True)], eryx.DEFAULT_HIDDEN)
    assert fs(served, "read", "README.md", offset=-1, len=4)[0]["error"] == "io"
    assert fs(served, "read", "README.md", offset=0, len=-1)[0]["error"] == "io"
    assert answer(served, {"op_id": "1", "op": "read", "mount": "/mnt/repo", "path": None})[0][
        "error"
    ] == "io"
    assert fs(served, "read", "src/big.bin", offset=0, len=2**40)[1] == bytes(100_000)
    # Hidden names ignore case, as macOS's filesystem does
    assert fs(served, "stat", ".ENV")[0]["error"] == "not_found"


def test_listings_inline_at_most_a_megabyte(tmp_path: Path) -> None:
    for i in range(20):
        (tmp_path / f"{i:02}.bin").write_bytes(bytes(60_000))
    served = mounts([(str(tmp_path), "/mnt/d", True)], ())
    reply, data = answer(served, {"op_id": "1", "op": "list", "mount": "/mnt/d", "path": ""})
    inline = [e for e in reply["entries"] if e.get("inline")]
    assert len(inline) == 17 and len(data) == 17 * 60_000


def test_volume_arguments_are_checked(repo: Path) -> None:
    with pytest.raises(TypeError, match="list of patterns"):
        sandbox(volumes=[(str(repo), "/mnt/repo", True)], hidden=".env")
    with pytest.raises(ValueError, match="two volumes"):
        sandbox(volumes=[(str(repo), "/mnt/repo", True), (str(repo), "/mnt/repo", True)])
