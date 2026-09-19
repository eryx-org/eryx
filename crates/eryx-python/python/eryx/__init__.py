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
    InitializationError: Error during sandbox initialization.
    ResourceLimitError: Resource limit exceeded during execution.
    TimeoutError: Execution timed out (also accessible as eryx.TimeoutError).
"""

from eryx._eryx import (
    # Classes
    CallbackRegistry,
    ExecuteResult,
    MCPManager,
    NetConfig,
    PooledSandbox,
    PoolStats,
    ResourceLimits,
    Sandbox,
    SandboxFactory,
    SandboxPool,
    Session,
    VfsStorage,
    # Exceptions
    EryxError,
    ExecutionError,
    InitializationError,
    PoolClosedError,
    PoolError,
    PoolExhaustedError,
    PoolTimeoutError,
    ResourceLimitError,
    TimeoutError,
    # Module metadata
    __version__,
)

__all__ = [
    # Classes
    "CallbackRegistry",
    "ExecuteResult",
    "MCPManager",
    "NetConfig",
    "PooledSandbox",
    "PoolStats",
    "ResourceLimits",
    "Sandbox",
    "SandboxFactory",
    "SandboxPool",
    "Session",
    "VfsStorage",
    # Exceptions
    "EryxError",
    "ExecutionError",
    "InitializationError",
    "PoolClosedError",
    "PoolError",
    "PoolExhaustedError",
    "PoolTimeoutError",
    "ResourceLimitError",
    "TimeoutError",
    # Metadata
    "__version__",
]
