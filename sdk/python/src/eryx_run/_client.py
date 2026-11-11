"""The hosted API: HTTP for plain runs and sessions, ``/v1/stream`` for runs
with callbacks."""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any, Optional

import httpx
import websockets

from ._callbacks import CallbackDict, invoke
from ._errors import (
    APIConnectionError,
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


class Connection:
    """Where to send requests, and the key to send with them."""

    def __init__(self, api_key: Optional[str], base_url: Optional[str]) -> None:
        api_key = api_key or os.environ.get("ERYX_API_KEY")
        if not api_key:
            raise ValueError(
                "No API key: pass api_key= or set ERYX_API_KEY "
                "(create one at https://app.eryx.run)"
            )
        self.api_key = api_key
        self.base_url = (base_url or os.environ.get("ERYX_URL") or DEFAULT_URL).rstrip("/")

    @property
    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.api_key}"}

    async def request(
        self, method: str, path: str, body: Optional[dict[str, Any]] = None
    ) -> Any:
        """Send a request and return its JSON body, raising ``APIError`` for an
        error response."""
        # ponytail: a client per request; share one if connection setup shows
        # up in latency
        try:
            async with httpx.AsyncClient(
                base_url=self.base_url, headers=self._headers, timeout=_HTTP_TIMEOUT
            ) as client:
                response = await client.request(method, path, json=body)
        except httpx.HTTPError as e:
            raise APIConnectionError(f"Couldn't reach {self.base_url}: {e}") from e
        if response.is_success:
            return response.json() if response.content else None
        raise _error_response(response)

    async def stream(
        self, body: dict[str, Any], callbacks: dict[str, CallbackDict]
    ) -> dict[str, Any]:
        """Run ``body`` (an ``execute`` message) over ``/v1/stream``, answering
        the code's callbacks, and return the ``completed`` message."""
        url = "ws" + self.base_url.removeprefix("http") + "/v1/stream"
        message = {
            "type": "execute",
            **body,
            "callbacks": [
                {"name": name, "description": cb.get("description", "")}
                for name, cb in callbacks.items()
            ],
        }
        try:
            async with websockets.connect(
                url,
                additional_headers=self._headers,
                subprotocols=[websockets.Subprotocol(PROTOCOL)],
                max_size=None,
            ) as ws:
                await ws.send(json.dumps(message))
                return await _serve(ws, callbacks)
        except websockets.InvalidStatus as e:
            raise _error_response(e.response) from None
        except websockets.ConnectionClosed as e:
            raise APIConnectionError(f"The connection closed before the run completed: {e}") from e
        except (OSError, websockets.InvalidHandshake) as e:
            raise APIConnectionError(f"Couldn't reach {url}: {e}") from e


async def _serve(
    ws: websockets.ClientConnection, callbacks: dict[str, CallbackDict]
) -> dict[str, Any]:
    """Answer callbacks until the run completes."""
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
