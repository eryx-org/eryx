"""Type stubs for the eryx native module."""

import builtins
from typing import Optional


class ExecuteResult:
    """Result of executing Python code in the sandbox."""

    @property
    def stdout(self) -> str:
        """Complete stdout output from the sandboxed code."""
        ...

    @property
    def duration_ms(self) -> float:
        """Execution duration in milliseconds."""
        ...

    @property
    def callback_invocations(self) -> int:
        """Number of callback invocations during execution."""
        ...

    @property
    def peak_memory_bytes(self) -> Optional[int]:
        """Peak memory usage in bytes (if available)."""
        ...


class ResourceLimits:
    """Resource limits for sandbox execution.

    Use this class to configure execution timeouts, memory limits,
    and callback restrictions for a sandbox.

    Example:
        limits = ResourceLimits(
            execution_timeout_ms=5000,  # 5 second timeout
            max_memory_bytes=100_000_000,  # 100MB memory limit
        )
        sandbox = Sandbox(resource_limits=limits)
    """

    execution_timeout_ms: Optional[int]
    """Maximum execution time in milliseconds."""

    callback_timeout_ms: Optional[int]
    """Maximum time for a single callback invocation in milliseconds."""

    max_memory_bytes: Optional[int]
    """Maximum memory usage in bytes."""

    max_callback_invocations: Optional[int]
    """Maximum number of callback invocations."""

    def __init__(
        self,
        *,
        execution_timeout_ms: Optional[int] = None,
        callback_timeout_ms: Optional[int] = None,
        max_memory_bytes: Optional[int] = None,
        max_callback_invocations: Optional[int] = None,
    ) -> None:
        """Create new resource limits.

        All parameters are optional. If not specified, defaults are used:
        - execution_timeout_ms: 30000 (30 seconds)
        - callback_timeout_ms: 10000 (10 seconds)
        - max_memory_bytes: 134217728 (128 MB)
        - max_callback_invocations: 1000

        Pass `None` to disable a specific limit.
        """
        ...

    @staticmethod
    def unlimited() -> ResourceLimits:
        """Create resource limits with no restrictions.

        Warning: Use with caution! Code can run indefinitely and use unlimited memory.
        """
        ...


class Sandbox:
    """A Python sandbox powered by WebAssembly.

    The Sandbox executes Python code in complete isolation from the host system.
    Each sandbox has its own memory space and cannot access files, network,
    or other system resources unless explicitly provided via callbacks.

    Example:
        sandbox = Sandbox()
        result = sandbox.execute('print("Hello from the sandbox!")')
        print(result.stdout)  # "Hello from the sandbox!\\n"
    """

    def __init__(
        self,
        *,
        resource_limits: Optional[ResourceLimits] = None,
    ) -> None:
        """Create a new sandbox with the embedded Python runtime.

        Args:
            resource_limits: Optional resource limits for execution.

        Raises:
            InitializationError: If the sandbox fails to initialize.
        """
        ...

    def execute(self, code: str) -> ExecuteResult:
        """Execute Python code in the sandbox.

        The code runs in complete isolation. Any output to stdout is captured
        and returned in the result.

        Args:
            code: Python source code to execute.

        Returns:
            ExecuteResult containing stdout, timing info, and statistics.

        Raises:
            ExecutionError: If the Python code raises an exception.
            TimeoutError: If execution exceeds the timeout limit.
            ResourceLimitError: If a resource limit is exceeded.

        Example:
            result = sandbox.execute('''
            x = 2 + 2
            print(f"2 + 2 = {x}")
            ''')
            print(result.stdout)  # "2 + 2 = 4\\n"
        """
        ...


class EryxError(Exception):
    """Base exception for all Eryx errors."""

    ...


class ExecutionError(EryxError):
    """Error during Python code execution in the sandbox."""

    ...


class InitializationError(EryxError):
    """Error during sandbox initialization."""

    ...


class ResourceLimitError(EryxError):
    """Resource limit exceeded during execution."""

    ...


class TimeoutError(builtins.TimeoutError, EryxError):
    """Execution timed out.

    This exception inherits from both Python's built-in TimeoutError
    and EryxError, so it can be caught with either.
    """

    ...


__version__: str
"""Version of the eryx package."""
