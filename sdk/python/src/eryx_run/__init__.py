"""eryx.run: run Python in an eryx sandbox, hosted.

The same API as pyeryx (``import eryx``) for what both can do, so moving
between running sandboxes yourself and on eryx.run is an import:

    >>> import eryx_run as eryx
    >>> sandbox = eryx.Sandbox()  # uses $ERYX_API_KEY
    >>> result = sandbox.execute("print(2 + 2)")
    >>> print(result.stdout_text)
    4

Callbacks work as in pyeryx: sandboxed code ``await``s them, and they run
here, in your process. So do ``volumes``: their directories are served from
here, read-only, as the code reads them.
"""

from ._callbacks import CallbackDict, CallbackRegistry
from ._client import DEFAULT_URL, PROTOCOL
from ._mounts import DEFAULT_HIDDEN
from ._errors import (
    APIConnectionError,
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

__version__ = "0.2.0"

__all__ = [
    "APIConnectionError",
    "APIError",
    "AuthenticationError",
    "CallbackDict",
    "CallbackRegistry",
    "CancelledError",
    "ConflictError",
    "DEFAULT_HIDDEN",
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
