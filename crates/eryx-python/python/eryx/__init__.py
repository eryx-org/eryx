"""
Eryx: A Python sandbox powered by WebAssembly.

This package provides a secure sandbox for executing untrusted Python code.
The sandbox runs Python inside WebAssembly, providing complete isolation
from the host system.

Example:
    >>> import eryx
    >>> sandbox = eryx.Sandbox()
    >>> result = sandbox.execute('print("Hello from the sandbox!")')
    >>> print(result.stdout)
    Hello from the sandbox!

Classes:
    Sandbox: The main sandbox class for executing Python code.
    ExecuteResult: Result of sandbox execution with stdout and stats.
    ResourceLimits: Configuration for execution limits.

Exceptions:
    EryxError: Base exception for all Eryx errors.
    ExecutionError: Error during Python code execution.
    InitializationError: Error during sandbox initialization.
    ResourceLimitError: Resource limit exceeded during execution.
    TimeoutError: Execution timed out (also accessible as eryx.TimeoutError).
"""

from eryx._eryx import (
    # Exceptions
    EryxError,
    ExecuteResult,
    ExecutionError,
    InitializationError,
    PreInitializedRuntime,
    ResourceLimitError,
    ResourceLimits,
    # Classes
    Sandbox,
    TimeoutError,
    # Module metadata
    __version__,
)

__all__ = [
    # Classes
    "Sandbox",
    "PreInitializedRuntime",
    "ExecuteResult",
    "ResourceLimits",
    # Exceptions
    "EryxError",
    "ExecutionError",
    "InitializationError",
    "ResourceLimitError",
    "TimeoutError",
    # Metadata
    "__version__",
]
