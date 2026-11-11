"""eryx.run: run Python in an eryx sandbox, hosted.

The same API as pyeryx (``import eryx``) for what both can do, so moving
between running sandboxes yourself and on eryx.run is an import:

    >>> import eryx_run as eryx
    >>> sandbox = eryx.Sandbox()  # uses $ERYX_API_KEY
    >>> result = sandbox.execute("print(2 + 2)")
    >>> print(result.stdout_text)
    4

Callbacks work as in pyeryx: sandboxed code ``await``s them, and they run
here, in your process.
"""

from ._callbacks import CallbackDict, CallbackRegistry
from ._client import DEFAULT_URL, PROTOCOL
from ._errors import (
    APIError,
    AuthenticationError,
    CancelledError,
    ConflictError,
    EryxError,
    ExecutionError,
    InitializationError,
    InvalidRequestError,
    NotFoundError,
    PythonExceptionError,
    RateLimitError,
    ResourceLimitError,
    ServiceUnavailableError,
    TimeoutError,
)
from ._sandbox import Sandbox, Session
from ._types import ExecuteResult, ResourceLimits

__version__ = "0.1.0"

__all__ = [
    "APIError",
    "AuthenticationError",
    "CallbackDict",
    "CallbackRegistry",
    "CancelledError",
    "ConflictError",
    "DEFAULT_URL",
    "EryxError",
    "ExecuteResult",
    "ExecutionError",
    "InitializationError",
    "InvalidRequestError",
    "NotFoundError",
    "PROTOCOL",
    "PythonExceptionError",
    "RateLimitError",
    "ResourceLimitError",
    "ResourceLimits",
    "Sandbox",
    "ServiceUnavailableError",
    "Session",
    "TimeoutError",
    "__version__",
]
