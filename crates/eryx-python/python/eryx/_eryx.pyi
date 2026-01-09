"""Type stubs for the eryx native module."""

import builtins
from pathlib import Path
from typing import Optional, Sequence, Union

PathLike = Union[str, Path]


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

    This creates a fast sandbox (~1-5ms) using the pre-initialized Python runtime.
    The sandbox has access to Python's standard library but no third-party packages.

    For sandboxes with custom packages, use `SandboxFactory` instead.

    Example:
        # Basic sandbox (stdlib only)
        sandbox = Sandbox()
        result = sandbox.execute('print("Hello from the sandbox!")')
        print(result.stdout)  # "Hello from the sandbox!"

        # For custom packages, use SandboxFactory:
        factory = SandboxFactory(
            packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
            imports=["jinja2"],
        )
        sandbox = factory.create_sandbox()
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

        Example:
            # Default sandbox (stdlib only)
            sandbox = Sandbox()
            result = sandbox.execute('import json; print(json.dumps([1, 2, 3]))')

            # Sandbox with resource limits
            sandbox = Sandbox(
                resource_limits=ResourceLimits(execution_timeout_ms=5000)
            )
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


class SandboxFactory:
    """A factory for creating sandboxes with custom packages.

    SandboxFactory bundles packages and pre-imports into a reusable snapshot,
    allowing fast creation of sandboxes with those packages already loaded.

    Note: For basic usage without packages, `eryx.Sandbox()` is already fast
    because the base runtime ships pre-initialized. Use `SandboxFactory` when
    you need to bundle custom packages.

    Example:
        # Create a factory with jinja2
        factory = SandboxFactory(
            packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
            imports=["jinja2"],
        )

        # Create sandboxes with packages already loaded (~10-20ms each)
        sandbox = factory.create_sandbox()
        result = sandbox.execute('from jinja2 import Template; ...')

        # Save for reuse across processes
        factory.save("/path/to/jinja2-factory.bin")

        # Load in another process
        factory = SandboxFactory.load("/path/to/jinja2-factory.bin")
    """

    @property
    def size_bytes(self) -> int:
        """Size of the pre-compiled runtime in bytes."""
        ...

    def __init__(
        self,
        *,
        site_packages: Optional[PathLike] = None,
        packages: Optional[Sequence[PathLike]] = None,
        imports: Optional[Sequence[str]] = None,
    ) -> None:
        """Create a new sandbox factory with custom packages.

        This performs one-time initialization that can take 3-5 seconds,
        but subsequent sandbox creation will be very fast (~10-20ms).

        Args:
            site_packages: Optional path to a directory containing Python packages.
            packages: Optional list of paths to .whl or .tar.gz package files.
                These are extracted and their native extensions are linked.
            imports: Optional list of module names to pre-import during initialization.
                Pre-imported modules are immediately available without import overhead.

        Raises:
            InitializationError: If initialization fails.

        Example:
            # Create factory with jinja2 and markupsafe
            factory = SandboxFactory(
                packages=[
                    "/path/to/jinja2-3.1.2-py3-none-any.whl",
                    "/path/to/markupsafe-2.1.3-wasi.tar.gz",
                ],
                imports=["jinja2"],
            )
        """
        ...

    @staticmethod
    def load(
        path: PathLike,
        *,
        site_packages: Optional[PathLike] = None,
    ) -> SandboxFactory:
        """Load a sandbox factory from a file.

        This loads a previously saved factory, which is much faster than
        creating a new one (~10ms vs ~3-5s).

        Args:
            path: Path to the saved factory file.
            site_packages: Optional path to site-packages directory.
                Required if the factory was saved without embedded packages.

        Returns:
            A SandboxFactory loaded from the file.

        Raises:
            InitializationError: If loading fails.

        Example:
            factory = SandboxFactory.load("/path/to/jinja2-factory.bin")
            sandbox = factory.create_sandbox()
        """
        ...

    def save(self, path: PathLike) -> None:
        """Save the sandbox factory to a file.

        The saved file can be loaded later with `SandboxFactory.load()`,
        which is much faster than creating a new factory.

        Args:
            path: Path where the factory should be saved.

        Raises:
            InitializationError: If saving fails.

        Example:
            factory = SandboxFactory(packages=[...], imports=["jinja2"])
            factory.save("/path/to/jinja2-factory.bin")
        """
        ...

    def create_sandbox(
        self,
        *,
        site_packages: Optional[PathLike] = None,
        resource_limits: Optional[ResourceLimits] = None,
    ) -> Sandbox:
        """Create a new sandbox from this factory.

        This is fast (~10-20ms) because the packages are already bundled
        into the factory's snapshot.

        Args:
            site_packages: Optional path to additional site-packages.
                If not provided, uses the site-packages from initialization.
            resource_limits: Optional resource limits for the sandbox.

        Returns:
            A new Sandbox ready to execute Python code.

        Raises:
            InitializationError: If sandbox creation fails.

        Example:
            sandbox = preinit.create_sandbox()
            result = sandbox.execute('print("Hello!")')
        """
        ...

    def to_bytes(self) -> bytes:
        """Get the pre-compiled runtime as bytes.

        This can be used for custom serialization or inspection.
        """
        ...


__version__: str
"""Version of the pyeryx package."""
