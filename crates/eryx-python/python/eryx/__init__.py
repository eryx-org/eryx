"""
Eryx: A Python sandbox powered by WebAssembly.

This package provides a secure sandbox for executing untrusted Python code.
The sandbox runs Python inside WebAssembly, providing complete isolation
from the host system.

Example:
    >>> import eryx
    >>> sandbox = eryx.Sandbox()
    >>> result = sandbox.execute('print("Hello from the sandbox!")')
    >>> print(result.stdout_text)
    Hello from the sandbox!

Classes:
    Sandbox: The main sandbox class for executing Python code.
    ExecuteResult: Result of sandbox execution with stdout and stats.
    ResourceLimits: Configuration for execution limits.
    NetConfig: Configuration for network access.

Exceptions:
    EryxError: Base exception for all Eryx errors.
    ExecutionError: Error during Python code execution.
    PythonExceptionError: The sandboxed code raised (subclass of ExecutionError).
    InitializationError: Error during sandbox initialization.
    ResourceLimitError: Resource limit exceeded during execution.
    TimeoutError: Execution timed out (subclass of both EryxError and the
        built-in TimeoutError).
"""

from eryx._eryx import (
    # Classes
    CallbackRegistry,
    CancelToken,
    ExecuteResult,
    MCPManager,
    NetConfig,
    PooledSandbox,
    PoolStats,
    ReplayOutcome,
    ResourceLimits,
    Sandbox,
    SandboxFactory,
    SandboxPool,
    Session,
    StateSnapshot,
    SuspendedCallback,
    VfsStorage,
    # Exceptions
    CancelledError,
    EryxError,
    ExecutionError,
    InitializationError,
    PoolClosedError,
    PoolError,
    PoolTimeoutError,
    PythonExceptionError,
    ResourceLimitError,
    SuspendCallback,
    TimeoutError,
    # Functions
    configure_shared_engine,
    # Module metadata
    __version__,
)

__all__ = [
    # Classes
    "CallbackRegistry",
    "CancelToken",
    "ExecuteResult",
    "MCPManager",
    "NetConfig",
    "PooledSandbox",
    "PoolStats",
    "ReplayOutcome",
    "ResourceLimits",
    "Sandbox",
    "SandboxFactory",
    "SandboxPool",
    "Session",
    "StateSnapshot",
    "SuspendedCallback",
    "VfsStorage",
    # Exceptions
    "CancelledError",
    "EryxError",
    "ExecutionError",
    "InitializationError",
    "PoolClosedError",
    "PoolError",
    "PoolTimeoutError",
    "PythonExceptionError",
    "ResourceLimitError",
    "SuspendCallback",
    "TimeoutError",
    # Functions
    "configure_shared_engine",
    # Metadata
    "__version__",
]
