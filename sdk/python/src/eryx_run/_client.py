"""The hosted API: HTTP for plain runs and sessions, ``/v1/stream`` for runs
with callbacks or volumes."""

from __future__ import annotations

import asyncio
import json
import os
import time
from dataclasses import dataclass, field
from typing import Any, Optional

import httpx
import websockets
from websockets.protocol import State

from ._callbacks import CallbackDict, invoke
from ._mounts import Mount, answer as answer_fs
from ._errors import (
    APIConnectionError,
    APIError,
    EryxError,
    ExecutionError,
    PythonExceptionError,
    RateLimitError,
    ResourceLimitError,
    TimeoutError,
    api_error,
)
from ._types import ExecuteResult

DEFAULT_URL = "https://api.eryx.run"

#: The ``/v1/stream`` protocol version this client speaks
PROTOCOL = "eryx.v1"

#: How long to wait for the API to answer, beyond the run's own time
_HTTP_TIMEOUT = httpx.Timeout(30.0, read=None)

#: How long an idle ``/v1/stream`` connection is kept for the next run. Under
#: proxies' idle timeouts, so a kept one is still open when it's used.
IDLE_STREAM_SECS = 15.0


@dataclass
class _Pool:
    """Connections for one event loop: an HTTP client (which keeps its own
    connections alive) and the idle ``/v1/stream`` sockets."""

    client: httpx.AsyncClient
    idle: list[tuple[websockets.ClientConnection, float]] = field(default_factory=list)

    def take(self) -> Optional[websockets.ClientConnection]:
        """The most recently used idle socket that's still fresh, dropping the
        rest that aren't."""
        now = time.monotonic()
        while self.idle:
            ws, since = self.idle.pop()
            if now - since < IDLE_STREAM_SECS and ws.state is State.OPEN:
                return ws
            _drop(ws)
        return None


def _drop(ws: websockets.ClientConnection) -> None:
    """Close a socket without waiting for the server's reply."""
    ws.transport.abort()


class Connection:
    """Where to send requests, and the key to send with them.

    Connections are kept for reuse, per event loop (an ``httpx`` client or a
    socket can't move between loops): sync calls all run on one loop (see
    ``_sandbox._run_sync``), so they share them.
    """

    def __init__(self, api_key: Optional[str], base_url: Optional[str]) -> None:
        api_key = api_key or os.environ.get("ERYX_API_KEY")
        if not api_key:
            raise ValueError(
                "No API key: pass api_key= or set ERYX_API_KEY "
                "(create one at https://app.eryx.run)"
            )
        self.api_key = api_key
        self.base_url = (base_url or os.environ.get("ERYX_URL") or DEFAULT_URL).rstrip("/")
        self._pools: dict[asyncio.AbstractEventLoop, _Pool] = {}

    def _pool(self) -> _Pool:
        """This event loop's connections."""
        loop = asyncio.get_running_loop()
        # A loop that's gone took its connections with it
        for closed in [other for other in self._pools if other.is_closed()]:
            del self._pools[closed]
        pool = self._pools.get(loop)
        if pool is None:
            client = httpx.AsyncClient(
                base_url=self.base_url, headers=self._headers, timeout=_HTTP_TIMEOUT
            )
            pool = self._pools[loop] = _Pool(client)
        return pool

    async def aclose(self) -> None:
        """Close this event loop's connections."""
        pool = self._pools.pop(asyncio.get_running_loop(), None)
        if pool is not None:
            for ws, _ in pool.idle:
                _drop(ws)
            await pool.client.aclose()

    @property
    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.api_key}"}

    async def request(
        self, method: str, path: str, body: Optional[dict[str, Any]] = None
    ) -> Any:
        """Send a request and return its JSON body, raising ``APIError`` for an
        error response."""
        try:
            response = await self._pool().client.request(method, path, json=body)
        except httpx.HTTPError as e:
            raise APIConnectionError(f"Couldn't reach {self.base_url}: {e}") from e
        if response.is_success:
            return response.json() if response.content else None
        raise _error_response(response)

    async def stream(
        self,
        body: dict[str, Any],
        callbacks: dict[str, CallbackDict],
        mounts: Optional[dict[str, Mount]] = None,
    ) -> dict[str, Any]:
        """Run ``body`` (an ``execute`` message) over ``/v1/stream``, answering
        the code's callbacks and file operations on ``mounts``, and return the
        ``completed`` message.

        A socket goes back to the pool after a run that completed or was
        refused (the server keeps it open), and is dropped otherwise. A run on
        a kept socket that fails isn't retried: the server may have started
        it.
        """
        mounts = mounts or {}
        url = "ws" + self.base_url.removeprefix("http") + "/v1/stream"
        message = {
            "type": "execute",
            **body,
            "callbacks": [
                {"name": name, "description": cb.get("description", "")}
                for name, cb in callbacks.items()
            ],
        }
        if mounts:
            message["mounts"] = [{"path": path} for path in mounts]
        pool = self._pool()
        ws = pool.take()
        try:
            if ws is None:
                ws = await websockets.connect(
                    url,
                    additional_headers=self._headers,
                    subprotocols=[websockets.Subprotocol(PROTOCOL)],
                    max_size=None,
                )
            await ws.send(json.dumps(message))
            completed = await _serve(ws, callbacks, mounts)
        except APIConnectionError:
            if ws is not None:
                _drop(ws)
            raise
        except APIError:
            # Refused before it ran; the socket's still good
            if ws is not None:
                pool.idle.append((ws, time.monotonic()))
            raise
        except websockets.InvalidStatus as e:
            raise _error_response(e.response) from None
        except websockets.ConnectionClosed as e:
            raise APIConnectionError(f"The connection closed before the run completed: {e}") from e
        except (OSError, websockets.InvalidHandshake) as e:
            raise APIConnectionError(f"Couldn't reach {url}: {e}") from e
        except BaseException:
            # Mid-run (e.g. cancelled): the server may still be running it
            if ws is not None:
                _drop(ws)
            raise
        pool.idle.append((ws, time.monotonic()))
        return completed


async def _serve(
    ws: websockets.ClientConnection,
    callbacks: dict[str, CallbackDict],
    mounts: dict[str, Mount],
) -> dict[str, Any]:
    """Answer callbacks and file operations until the run completes."""
    lock = asyncio.Lock()
    tasks: set[asyncio.Task[None]] = set()

    async def answer(call_id: str, name: str, args: dict[str, Any]) -> None:
        reply = {"type": "callback_response", "call_id": call_id}
        # A result JSON can't hold is an error too, so the code isn't left
        # waiting for an answer
        try:
            text = json.dumps({**reply, "result": await invoke(callbacks[name], args)})
        except Exception as e:  # the code sees it as a RuntimeError
            text = json.dumps({**reply, "error": f"{type(e).__name__}: {e}"})
        async with lock:
            try:
                await ws.send(text)
            except websockets.ConnectionClosed:
                pass  # the run is over; `_serve` reports why

    async def answer_file(message: dict[str, Any]) -> None:
        reply, data = await asyncio.to_thread(answer_fs, mounts, message)
        # The data must come straight after its reply
        async with lock:
            try:
                await ws.send(json.dumps(reply))
                if data:
                    await ws.send(data)
            except websockets.ConnectionClosed:
                pass

    try:
        async for raw in ws:
            message = json.loads(raw)
            kind = message.get("type")
            if kind == "callback":
                task = asyncio.create_task(
                    answer(message["call_id"], message["name"], message.get("args") or {})
                )
                tasks.add(task)
                task.add_done_callback(tasks.discard)
            elif kind == "fs":
                task = asyncio.create_task(answer_file(message))
                tasks.add(task)
                task.add_done_callback(tasks.discard)
            elif kind == "completed":
                return dict(message)
            elif kind == "error":
                raise api_error(message.get("message", ""), message.get("code"), None)
            # Other types are newer additions to the protocol: ignore them
    finally:
        for task in tasks:
            task.cancel()
    raise APIConnectionError("The connection closed before the run completed")


def _error_response(response: Any) -> EryxError:
    """The exception for an error response from ``httpx`` or ``websockets``."""
    status = response.status_code
    try:
        body = json.loads(response.body if hasattr(response, "body") else response.content)
        error = body["error"]
        message, code = error["message"], error["code"]
    except (ValueError, KeyError, TypeError):
        message, code = f"HTTP {status}", None
    exc = api_error(message, code, status)
    if isinstance(exc, RateLimitError):
        retry_after = response.headers.get("Retry-After")
        if retry_after is not None:
            try:
                exc.retry_after = float(retry_after)
            except ValueError:
                pass
    return exc


def finish(response: dict[str, Any]) -> ExecuteResult:
    """The result of a finished run, or the exception pyeryx would raise for
    it."""
    stdout = response.get("stdout", "").encode()
    stderr = response.get("stderr", "").encode()
    stats = {
        "duration_ms": float(response.get("duration_ms", 0)),
        "callback_invocations": response.get("callback_invocations", 0),
        "peak_memory_bytes": response.get("peak_memory_bytes"),
        "fuel_consumed": response.get("compute_units"),
    }
    status = response.get("status")
    if status == "success":
        return ExecuteResult(
            stdout=stdout,
            stderr=stderr,
            result_json=json.dumps(response["result"]) if "result" in response else None,
            result_error=response.get("result_error"),
            network_bytes=response.get("network_bytes"),
            trace_id=response.get("trace_id"),
            unsaved_variables=list(response.get("unsaved_variables", [])),
            **stats,
        )
    error = response.get("error") or f"run ended with status {status!r}"
    exc: EryxError
    if status == "timeout":
        exc = TimeoutError(error)
    elif status == "resource_limit":
        exc = ResourceLimitError(error)
    # ponytail: the API says `error` for both an exception in the code and a
    # failure around it, whose message is the traceback only for the former;
    # use a failure kind once the API sends one
    elif error.startswith("Traceback (most recent call last)"):
        exc = PythonExceptionError(error)
        exc.stdout, exc.stderr = stdout, stderr
    else:
        exc = ExecutionError(error)
    for name, value in stats.items():
        setattr(exc, name, value)
    raise exc
