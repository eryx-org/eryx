"""``Sandbox`` and ``Session``, with pyeryx's API, running on eryx.run."""

from __future__ import annotations

import asyncio
import threading
from urllib.parse import quote
from collections.abc import Coroutine, Sequence
from types import TracebackType
from typing import Any, Optional, TypeVar

from ._callbacks import Callbacks, normalize
from ._client import Connection, finish
from ._errors import ConflictError
from ._types import ExecuteResult, ResourceLimits

T = TypeVar("T")
R = TypeVar("R", bound="_Runner")

#: pyeryx arguments the hosted service has another way to do, and which
_LOCAL_ONLY = {
    "network": "use network_policies= (set up at https://app.eryx.run)",
    "mcp": "use connections= (set up at https://app.eryx.run)",
    "secrets": "add secrets to a network policy",
    "scrub_stdout": "secrets are scrubbed by their network policy",
    "scrub_stderr": "secrets are scrubbed by their network policy",
    "scrub_result": "secrets are scrubbed by their network policy",
    "scrub_files": "secrets are scrubbed by their network policy",
    "volumes": "mounting local directories isn't supported yet",
    "on_stdout": "streaming output isn't supported yet",
    "on_stderr": "streaming output isn't supported yet",
}


_loop: Optional[asyncio.AbstractEventLoop] = None
_loop_lock = threading.Lock()


def _sync_loop() -> asyncio.AbstractEventLoop:
    """The event loop sync calls run on: one, in a daemon thread, so they
    share their connections (which can't move between loops)."""
    global _loop
    with _loop_lock:
        if _loop is None:
            _loop = asyncio.new_event_loop()
            threading.Thread(target=_loop.run_forever, name="eryx-run", daemon=True).start()
        return _loop


def _run_sync(coro: Coroutine[Any, Any, T]) -> T:
    """Run ``coro`` to completion from sync code, even inside an event loop
    (where it blocks the loop, as pyeryx's sync methods do)."""
    loop = _sync_loop()
    try:
        running = asyncio.get_running_loop()
    except RuntimeError:
        running = None
    if running is loop:
        coro.close()
        raise RuntimeError("use the *_async methods from an async callback")
    return asyncio.run_coroutine_threadsafe(coro, loop).result()


class _Runner:
    """What ``Sandbox`` and ``Session`` share: where to run and with what."""

    def __init__(
        self,
        *,
        resource_limits: Optional[ResourceLimits],
        callbacks: Optional[Callbacks],
        environment: Optional[str],
        connections: Optional[Sequence[str]],
        network_policies: Optional[Sequence[str]],
        result_variable: Optional[str],
        api_key: Optional[str],
        base_url: Optional[str],
        local_only: dict[str, Any],
    ) -> None:
        for name, value in local_only.items():
            if name not in _LOCAL_ONLY:
                raise TypeError(f"unexpected keyword argument {name!r}")
            if value is not None:
                raise ValueError(f"{name}= is only for local sandboxes: {_LOCAL_ONLY[name]}")
        if result_variable not in (None, "result"):
            raise ValueError("eryx.run captures the `result` variable only")
        self._connection = Connection(api_key, base_url)
        self._callbacks = normalize(callbacks)
        self._limits = resource_limits or ResourceLimits()
        self._options: dict[str, Any] = {}
        if environment is not None:
            self._options["environment"] = environment
        if connections is not None:
            self._options["connections"] = list(connections)
        if network_policies is not None:
            self._options["network_policies"] = list(network_policies)

    def _body(self, code: str) -> dict[str, Any]:
        return {"code": code, **self._limits.hosted_request(), **self._options}

    @staticmethod
    def _check_cancel(cancel: object) -> None:
        if cancel is not None:
            raise ValueError("cancelling a hosted run isn't supported yet")

    def close(self) -> None:
        """Close the connections sync calls kept open for the next run.

        Optional: they close when the process ends. A later run opens new
        ones.
        """
        _run_sync(self._connection.aclose())

    async def aclose(self) -> None:
        """Close the connections this event loop's runs kept open."""
        await self._connection.aclose()

    def __enter__(self: R) -> R:
        return self

    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        self.close()

    async def __aenter__(self: R) -> R:
        return self

    async def __aexit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        await self.aclose()


class Sandbox(_Runner):
    """Runs Python in an isolated sandbox on eryx.run.

    Takes pyeryx's ``Sandbox`` arguments, plus where to run: ``api_key``
    (default ``$ERYX_API_KEY``) and ``base_url`` (default ``$ERYX_URL``, else
    ``https://api.eryx.run``). ``environment``, ``connections`` and
    ``network_policies`` pick an uploaded environment, MCP connections and
    network policies, as in the API. Each ``execute`` is a fresh run.

    Example:
        >>> import eryx_run as eryx
        >>> sandbox = eryx.Sandbox()
        >>> print(sandbox.execute('print("Hello from the sandbox!")').stdout_text)
        Hello from the sandbox!
    """

    def __init__(
        self,
        *,
        resource_limits: Optional[ResourceLimits] = None,
        callbacks: Optional[Callbacks] = None,
        result_variable: Optional[str] = None,
        environment: Optional[str] = None,
        connections: Optional[Sequence[str]] = None,
        network_policies: Optional[Sequence[str]] = None,
        api_key: Optional[str] = None,
        base_url: Optional[str] = None,
        **local_only: Any,
    ) -> None:
        super().__init__(
            resource_limits=resource_limits,
            callbacks=callbacks,
            environment=environment,
            connections=connections,
            network_policies=network_policies,
            result_variable=result_variable,
            api_key=api_key,
            base_url=base_url,
            local_only=local_only,
        )

    def execute(self, code: str, *, cancel: object = None) -> ExecuteResult:
        """Run ``code`` and return its result.

        Raises:
            PythonExceptionError: If the code raises an exception.
            TimeoutError: If the run exceeds its time limit.
            ResourceLimitError: If it uses up its compute units or memory.
            APIError: If eryx.run refuses the request.
        """
        return _run_sync(self.execute_async(code, cancel=cancel))

    async def execute_async(self, code: str, *, cancel: object = None) -> ExecuteResult:
        """Like ``execute``, from asyncio."""
        self._check_cancel(cancel)
        body = self._body(code)
        if self._callbacks:
            return finish(await self._connection.stream(body, self._callbacks))
        return finish(await self._connection.request("POST", "/v1/execute", body))


class Session(_Runner):
    """Keeps Python state between runs, on eryx.run: variables, functions,
    imports and files under ``/data``, like cells in a notebook.

    Takes ``Sandbox``'s arguments, plus ``ttl_secs`` (how long the session
    lasts after its last run; default an hour, at most a day) and ``id`` (an
    existing session to use, e.g. one another process created). The session
    is created on the first run; ``delete()`` ends it.

    Example:
        >>> session = eryx.Session()
        >>> session.execute("x = 1")
        >>> print(session.execute("print(x + 1)").stdout_text)
        2
    """

    def __init__(
        self,
        *,
        resource_limits: Optional[ResourceLimits] = None,
        callbacks: Optional[Callbacks] = None,
        result_variable: Optional[str] = None,
        environment: Optional[str] = None,
        connections: Optional[Sequence[str]] = None,
        network_policies: Optional[Sequence[str]] = None,
        ttl_secs: Optional[int] = None,
        id: Optional[str] = None,
        api_key: Optional[str] = None,
        base_url: Optional[str] = None,
        **local_only: Any,
    ) -> None:
        super().__init__(
            resource_limits=resource_limits,
            callbacks=callbacks,
            environment=environment,
            connections=connections,
            network_policies=network_policies,
            result_variable=result_variable,
            api_key=api_key,
            base_url=base_url,
            local_only=local_only,
        )
        self._ttl_secs = ttl_secs
        self._id = id
        self._execution_count = 0
        self._creating = threading.Lock()

    @property
    def id(self) -> Optional[str]:
        """The session's id, once it's created."""
        return self._id

    @property
    def execution_count(self) -> int:
        """Number of executions this object has run in the session."""
        return self._execution_count

    async def _ensure(self) -> str:
        """The session's id, creating it on the first run"""
        if self._id is not None:
            return self._id
        # Runs in a session go one at a time, so a second one while the first
        # creates it would be refused anyway; refusing it here also keeps it
        # from creating a second session
        if not self._creating.acquire(blocking=False):
            raise ConflictError("Runs in a session go one at a time", code="session_busy")
        try:
            body: dict[str, Any] = {}
            if "environment" in self._options:
                body["environment"] = self._options["environment"]
            if self._ttl_secs is not None:
                body["ttl_secs"] = self._ttl_secs
            created = await self._connection.request("POST", "/v1/sessions", body)
            self._id = str(created["id"])
            return self._id
        finally:
            self._creating.release()

    def execute(self, code: str, *, cancel: object = None) -> ExecuteResult:
        """Run ``code`` in the session and return its result. Raises as
        ``Sandbox.execute`` does; code that raises keeps its changes."""
        return _run_sync(self.execute_async(code, cancel=cancel))

    async def execute_async(self, code: str, *, cancel: object = None) -> ExecuteResult:
        """Like ``execute``, from asyncio."""
        self._check_cancel(cancel)
        session_id = await self._ensure()
        body = self._body(code)
        if self._callbacks:
            response = await self._connection.stream(
                {**body, "session_id": session_id}, self._callbacks
            )
        else:
            response = await self._connection.request(
                "POST", f"/v1/sessions/{quote(session_id, safe='')}/execute", body
            )
        self._execution_count += 1
        return finish(response)

    def delete(self) -> None:
        """End the session now, rather than when it expires."""
        _run_sync(self.delete_async())

    async def delete_async(self) -> None:
        """Like ``delete``, from asyncio."""
        if self._id is not None:
            await self._connection.request("DELETE", f"/v1/sessions/{quote(self._id, safe='')}")
            self._id = None

    def snapshot_state(self) -> Any:
        """Not available on eryx.run: the service keeps the state."""
        raise NotImplementedError("eryx.run keeps a session's state for you")

    def restore_state(self, snapshot: Any) -> None:
        """Not available on eryx.run: the service keeps the state."""
        raise NotImplementedError("eryx.run keeps a session's state for you")
