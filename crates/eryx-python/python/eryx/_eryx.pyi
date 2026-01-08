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

    Example:
        # Basic sandbox
        sandbox = Sandbox()
        result = sandbox.execute('print("Hello from the sandbox!")')
        print(result.stdout)  # "Hello from the sandbox!"

        # Sandbox with packages (e.g., jinja2)
        sandbox = Sandbox(
            packages=["/path/to/jinja2-3.1.2-py3-none-any.whl"],
        )
        result = sandbox.execute('from jinja2 import Template; print(Template("{{ x }}").render(x=42))')
    """

    def __init__(
        self,
        *,
        site_packages: Optional[Union[str, Path]] = None,
        packages: Optional[Sequence[Union[str, Path]]] = None,
        resource_limits: Optional[ResourceLimits] = None,
    ) -> None:
        """Create a new sandbox with the embedded Python runtime.

        Args:
            site_packages: Optional path to a directory containing Python packages.
                This directory will be mounted at `/site-packages` in the sandbox
                and added to Python's import path.
            packages: Optional list of paths to Python packages (.whl or .tar.gz files).
                Packages are extracted and their contents added to the sandbox.
                Packages with native extensions (.so files) are automatically
                late-linked into the WebAssembly component.
            resource_limits: Optional resource limits for execution.

        Raises:
            InitializationError: If the sandbox fails to initialize.

        Example:
            # Default sandbox (stdlib only)
            sandbox = Sandbox()

            # Sandbox with packages
            sandbox = Sandbox(
                packages=[
                    "/path/to/jinja2-3.1.2-py3-none-any.whl",
                    "/path/to/markupsafe-2.1.3-wasi.tar.gz",
                ]
            )

            # Sandbox with pre-extracted site-packages
            sandbox = Sandbox(site_packages="/path/to/site-packages")
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


class PreInitializedRuntime:
    """A pre-initialized Python runtime for fast sandbox creation.

    Pre-initialization runs Python's startup code and optionally imports
    specified modules, capturing the initialized state into a WASM component.
    This avoids the ~450ms Python initialization cost on each sandbox creation,
    reducing it to ~10-20ms.

    Example:
        # Create pre-initialized runtime with jinja2
        preinit = PreInitializedRuntime(
            site_packages="/path/to/site-packages",
            imports=["jinja2"],
        )

        # Create sandboxes quickly (~10-20ms each)
        sandbox = preinit.create_sandbox()
        result = sandbox.execute('from jinja2 import Template; ...')

        # Save for reuse across processes
        preinit.save("/path/to/runtime.bin")

        # Load in another process
        preinit = PreInitializedRuntime.load("/path/to/runtime.bin")
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
        """Create a new pre-initialized runtime.

        This performs one-time initialization that can take 3-5 seconds,
        but subsequent sandbox creation will be very fast (~10-20ms).

        Args:
            site_packages: Optional path to a directory containing Python packages.
            packages: Optional list of paths to .whl or .tar.gz package files.
                These are extracted and their native extensions are linked.
            imports: Optional list of module names to pre-import during initialization.
                Pre-imported modules are immediately available without import overhead.

        Raises:
            InitializationError: If pre-initialization fails.

        Example:
            # Pre-initialize with jinja2 and markupsafe
            preinit = PreInitializedRuntime(
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
    ) -> PreInitializedRuntime:
        """Load a pre-initialized runtime from a file.

        This loads a previously saved runtime, which is much faster than
        creating a new one (~10ms vs ~3-5s).

        Args:
            path: Path to the saved runtime file.
            site_packages: Optional path to site-packages directory.
                Required if the runtime was saved without embedded packages.

        Returns:
            A PreInitializedRuntime loaded from the file.

        Raises:
            InitializationError: If loading fails.

        Example:
            preinit = PreInitializedRuntime.load("/path/to/runtime.bin")
            sandbox = preinit.create_sandbox()
        """
        ...

    def save(self, path: PathLike) -> None:
        """Save the pre-initialized runtime to a file.

        The saved file can be loaded later with `PreInitializedRuntime.load()`,
        which is much faster than creating a new runtime.

        Args:
            path: Path where the runtime should be saved.

        Raises:
            InitializationError: If saving fails.

        Example:
            preinit = PreInitializedRuntime(imports=["json", "re"])
            preinit.save("/path/to/runtime.bin")
        """
        ...

    def create_sandbox(
        self,
        *,
        site_packages: Optional[PathLike] = None,
        resource_limits: Optional[ResourceLimits] = None,
    ) -> Sandbox:
        """Create a new sandbox from the pre-initialized runtime.

        This is very fast (~10-20ms) because the Python interpreter is
        already initialized in the WASM component.

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
"""Version of the eryx package."""
