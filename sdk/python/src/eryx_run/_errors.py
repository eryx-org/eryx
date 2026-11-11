"""Exceptions, shared with pyeryx.

The first group mirrors pyeryx's hierarchy, so code catching
``eryx.TimeoutError`` or ``eryx.PythonExceptionError`` works against either
package. A failed run's exception carries what it used, as on
``ExecuteResult``: ``duration_ms``, ``callback_invocations``,
``peak_memory_bytes`` and ``fuel_consumed`` (``None`` when unknown).

The ``APIError`` group is for requests the hosted API refused.
"""

from __future__ import annotations

import builtins
from typing import Optional


class EryxError(Exception):
    """Base exception for all Eryx errors."""

    duration_ms: Optional[float] = None
    callback_invocations: Optional[int] = None
    peak_memory_bytes: Optional[int] = None
    fuel_consumed: Optional[int] = None


class ExecutionError(EryxError):
    """Error during Python code execution in the sandbox."""


class PythonExceptionError(ExecutionError):
    """The sandboxed Python code raised an uncaught exception.

    ``stdout`` and ``stderr`` hold what the code wrote before it raised;
    ``stderr`` ends with the traceback.
    """

    stdout: bytes = b""
    stderr: bytes = b""


class InitializationError(EryxError):
    """Error during sandbox initialization."""


class ResourceLimitError(EryxError):
    """Resource limit exceeded during execution."""


class TimeoutError(builtins.TimeoutError, EryxError):
    """Execution timed out."""


class CancelledError(EryxError):
    """Execution was cancelled."""


class APIError(EryxError):
    """The hosted API refused a request.

    ``code`` is the API's error code (``"invalid_key"``, ``"session_busy"``,
    ...) and ``status_code`` the HTTP status. Errors sent over the stream have
    no status, and no code from servers older than eryx-run#138.
    """

    def __init__(
        self,
        message: str,
        *,
        code: Optional[str] = None,
        status_code: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.status_code = status_code


class AuthenticationError(APIError):
    """The API key is missing, malformed, wrong or revoked."""


class InvalidRequestError(APIError):
    """The request wasn't valid."""


class NotFoundError(APIError):
    """No such session, environment, connection or network policy."""


class ConflictError(APIError):
    """The session is busy, or the environment isn't ready or already exists."""


class RateLimitError(APIError):
    """Over a rate, concurrency or count limit.

    ``retry_after`` is how many seconds to wait, when the API said.
    """

    retry_after: Optional[float] = None


class ServiceUnavailableError(APIError):
    """The service is at capacity, or failed; retry after a short wait."""


_API_ERRORS: dict[str, type[APIError]] = {
    "missing_auth": AuthenticationError,
    "invalid_auth": AuthenticationError,
    "invalid_key": AuthenticationError,
    "invalid_request": InvalidRequestError,
    "not_found": NotFoundError,
    "conflict": ConflictError,
    "environment_not_ready": ConflictError,
    "session_busy": ConflictError,
    "rate_limit_exceeded": RateLimitError,
    "concurrency_limit": RateLimitError,
    "session_limit": RateLimitError,
    "environment_limit": RateLimitError,
    "connection_limit": RateLimitError,
    "network_policy_limit": RateLimitError,
    "unavailable": ServiceUnavailableError,
    "internal_error": ServiceUnavailableError,
}


def api_error(
    message: str, code: Optional[str], status_code: Optional[int]
) -> APIError:
    """The exception for an error response."""
    cls = _API_ERRORS.get(code or "", APIError)
    return cls(message, code=code, status_code=status_code)
