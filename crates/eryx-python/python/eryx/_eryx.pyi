"""Type stubs for the eryx native module."""

import builtins
from pathlib import Path
from typing import Any, Callable, Iterator, Optional, Sequence, TypedDict, Union

PathLike = Union[str, Path]


class SecretDict(TypedDict, total=False):
    """Dictionary format for configuring a secret.

    Example:
        {"value": "sk-real-key", "allowed_hosts": ["api.openai.com"]}
    """

    value: str
    """The secret value (required)."""

    allowed_hosts: list[str]
    """Optional list of hosts allowed to receive this secret. Defaults to []."""


class CallbackDict(TypedDict, total=False):
    """Dictionary format for defining callbacks."""

    name: str
    """The callback name (required)."""

    fn: Callable[..., Any]
    """The callback function (required)."""

    description: str
    """Optional description of what the callback does."""

    schema: dict[str, Any]
    """Optional JSON Schema for parameters."""


class MCPManager:
    """Manager for MCP (Model Context Protocol) server connections.

    Creates connections to MCP servers and exposes their tools as native
    Rust callbacks that can be used by ``Sandbox`` and ``Session``.

    Tools are exposed as dotted callbacks under ``mcp.<server>.<tool>``,
    so sandboxed code can call them as ``await mcp.server.tool(args)``.

    Example:
        manager = MCPManager()
        manager.connect("fs", "npx", ["-y", "@anthropic/mcp-server-filesystem", "."])
        tools = manager.list_tools()
        sandbox = Sandbox(mcp=manager)
        result = sandbox.execute('data = await mcp.fs.read_file(path="README.md")')
    """

    @property
    def server_names(self) -> list[str]:
        """Names of all connected MCP servers."""
        ...

    def __init__(self) -> None:
        """Create a new empty MCP manager."""
        ...

    def connect(
        self,
        name: str,
        command: str,
        args: list[str] = [],
        env: dict[str, str] = {},
        timeout_secs: float = 30.0,
    ) -> int:
        """Connect to an MCP server by spawning a child process.

        Args:
            name: A human-readable name for this server connection.
            command: The command to spawn (e.g., "npx", "uvx").
            args: Arguments to pass to the command.
            env: Environment variables to set for the child process.
            timeout_secs: Timeout in seconds for the connection handshake.

        Returns:
            The number of tools available from this server.

        Raises:
            InitializationError: If the connection fails.
        """
        ...

    def list_tools(self) -> list[dict[str, Any]]:
        """List all available tools across all connected servers.

        Returns:
            A list of dicts, each with "name", "description", and "schema" keys.
        """
        ...

    def close(self) -> None:
        """Gracefully shut down all MCP server connections."""
        ...


class CallbackRegistry:
    """A registry for collecting callbacks using the decorator pattern.

    Example:
        registry = eryx.CallbackRegistry()

        @registry.callback(description="Returns current timestamp")
        def get_time():
            import time
            return {"timestamp": time.time()}

        @registry.callback(name="echo", description="Echoes the message")
        def my_echo_fn(message: str, repeat: int = 1):
            return {"echoed": message * repeat}

        sandbox = eryx.Sandbox(callbacks=registry)
    """

    def __init__(self) -> None:
        """Create a new empty callback registry."""
        ...

    def callback(
        self,
        name: Optional[str] = None,
        description: Optional[str] = None,
        schema: Optional[dict[str, Any]] = None,
    ) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """Decorator to register a callback function.

        Args:
            name: Optional name for the callback. Defaults to the function's __name__.
            description: Optional description. Defaults to the function's __doc__ or empty string.
            schema: Optional JSON Schema dict for parameters. Auto-inferred if not provided.

        Returns:
            A decorator that registers the function and returns it unchanged.

        Example:
            @registry.callback(description="Echoes the message")
            def echo(message: str, repeat: int = 1):
                return {"echoed": message * repeat}
        """
        ...

    def add(
        self,
        fn: Callable[..., Any],
        *,
        name: Optional[str] = None,
        description: Optional[str] = None,
        schema: Optional[dict[str, Any]] = None,
    ) -> None:
        """Add a callback directly without using the decorator pattern.

        Args:
            fn: The callable to register.
            name: Optional name. Defaults to fn.__name__.
            description: Optional description. Defaults to fn.__doc__ or empty string.
            schema: Optional JSON Schema dict.
        """
        ...

    def __len__(self) -> int:
        """Return the number of registered callbacks."""
        ...

    def __iter__(self) -> Iterator[CallbackDict]:
        """Return an iterator over the registered callbacks as dicts."""
        ...


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


class NetConfig:
    """Network configuration for sandbox execution.

    Use this class to configure which hosts Python code can connect to,
    set timeouts, and add custom certificates.

    By default, network access is **disabled**. To enable networking,
    create a `NetConfig` and pass it to the Sandbox.

    Example:
        # Allow connections to external APIs only
        net = NetConfig(
            allowed_hosts=["api.example.com", "*.googleapis.com"],
        )
        sandbox = Sandbox(network=net)

        # Allow localhost for local development
        net = NetConfig.permissive()
        sandbox = Sandbox(network=net)
    """

    max_connections: int
    """Maximum concurrent connections."""

    connect_timeout_ms: int
    """Connection timeout in milliseconds."""

    io_timeout_ms: int
    """I/O timeout in milliseconds."""

    allowed_hosts: list[str]
    """Allowed host patterns (empty = allow all external hosts)."""

    blocked_hosts: list[str]
    """Blocked host patterns."""

    def __init__(
        self,
        *,
        max_connections: int = 10,
        connect_timeout_ms: int = 30000,
        io_timeout_ms: int = 60000,
        allowed_hosts: Optional[Sequence[str]] = None,
        blocked_hosts: Optional[Sequence[str]] = None,
    ) -> None:
        """Create new network configuration.

        By default:
        - max_connections: 10
        - connect_timeout_ms: 30000 (30 seconds)
        - io_timeout_ms: 60000 (60 seconds)
        - allowed_hosts: [] (allow all external hosts)
        - blocked_hosts: localhost and private networks

        Args:
            max_connections: Maximum number of concurrent connections.
            connect_timeout_ms: Timeout for establishing connections.
            io_timeout_ms: Timeout for read/write operations.
            allowed_hosts: List of allowed host patterns (supports wildcards like "*.example.com").
            blocked_hosts: List of blocked host patterns.

        Example:
            # Only allow specific APIs
            net = NetConfig(allowed_hosts=["api.example.com", "*.openai.com"])

            # Custom timeouts
            net = NetConfig(connect_timeout_ms=5000, io_timeout_ms=10000)
        """
        ...

    @staticmethod
    def permissive() -> NetConfig:
        """Create a permissive config that allows all hosts including localhost.

        Warning: This allows sandbox code to access local services. Use with caution.

        Example:
            net = NetConfig.permissive()
            sandbox = Sandbox(network=net)
        """
        ...

    def allow_host(self, pattern: str) -> NetConfig:
        """Add a host pattern to the allowed list.

        Patterns support wildcards: `*.example.com`, `api.*.com`

        Returns self for method chaining.

        Example:
            net = NetConfig().allow_host("api.example.com").allow_host("*.openai.com")
        """
        ...

    def block_host(self, pattern: str) -> NetConfig:
        """Add a host pattern to the blocked list.

        Returns self for method chaining.
        """
        ...

    def allow_localhost(self) -> NetConfig:
        """Allow connections to localhost (disabled by default).

        Returns self for method chaining.

        Example:
            net = NetConfig().allow_localhost()
        """
        ...

    def with_root_cert(self, cert_der: bytes) -> NetConfig:
        """Add a custom root certificate (DER-encoded bytes).

        This is useful for testing with self-signed certificates.

        Args:
            cert_der: The certificate in DER format as bytes.

        Returns self for method chaining.
        """
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

        # Sandbox with callbacks
        def get_time():
            import time
            return {"timestamp": time.time()}

        sandbox = Sandbox(callbacks=[
            {"name": "get_time", "fn": get_time, "description": "Returns current time"}
        ])
        result = sandbox.execute('t = await get_time(); print(t)')

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
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        secrets: Optional[dict[str, SecretDict]] = None,
        scrub_stdout: Optional[bool] = None,
        scrub_stderr: Optional[bool] = None,
        scrub_files: Optional[bool] = None,
        volumes: Optional[Sequence[tuple[str, str, bool]]] = None,
        on_stdout: Optional[Callable[[str], None]] = None,
        on_stderr: Optional[Callable[[str], None]] = None,
    ) -> None:
        """Create a new sandbox with the embedded Python runtime.

        Args:
            resource_limits: Optional resource limits for execution.
            network: Optional network configuration. If provided, enables networking.
            callbacks: Optional callbacks that sandboxed code can invoke.
                Can be a CallbackRegistry or a list of callback dicts with
                "name", "fn", and optionally "description" and "schema" keys.
            mcp: Optional MCPManager with connected MCP servers. Tools from
                connected servers are exposed as callbacks named
                ``mcp.<server>.<tool>``.
            secrets: Optional dict of secret names to secret configurations.
                Each value is a dict with "value" (required) and "allowed_hosts"
                (optional, defaults to []).
                Python code sees a placeholder instead of the real value.
                The real secret is only substituted when writing to an allowed host.
            scrub_stdout: Whether to scrub secret placeholders from stdout.
                Defaults to True when secrets are provided.
            scrub_stderr: Whether to scrub secret placeholders from stderr.
                Defaults to True when secrets are provided.
            scrub_files: Whether to scrub secret placeholders from file writes.
                Defaults to True when secrets are provided.
            on_stdout: Optional callback for streaming stdout output.
                Called in real-time as Python code writes to stdout, rather than
                waiting for execution to complete. Receives the text chunk as a string.
            on_stderr: Optional callback for streaming stderr output.
                Called in real-time as Python code writes to stderr, rather than
                waiting for execution to complete. Receives the text chunk as a string.

        Raises:
            InitializationError: If the sandbox fails to initialize.

        Example:
            # Default sandbox (stdlib only, no network)
            sandbox = Sandbox()
            result = sandbox.execute('import json; print(json.dumps([1, 2, 3]))')

            # Sandbox with MCP tools
            manager = MCPManager()
            manager.connect("fs", "npx", ["-y", "@anthropic/mcp-server-filesystem", "."])
            sandbox = Sandbox(mcp=manager)

            # Sandbox with callbacks (registry-based)
            registry = CallbackRegistry()

            @registry.callback(description="Echoes the message")
            def echo(message: str):
                return {"echoed": message}

            sandbox = Sandbox(callbacks=registry)
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
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        secrets: Optional[dict[str, SecretDict]] = None,
        scrub_stdout: Optional[bool] = None,
        scrub_stderr: Optional[bool] = None,
        scrub_files: Optional[bool] = None,
        volumes: Optional[Sequence[tuple[str, str, bool]]] = None,
        on_stdout: Optional[Callable[[str], None]] = None,
        on_stderr: Optional[Callable[[str], None]] = None,
    ) -> Sandbox:
        """Create a new sandbox from this factory.

        This is fast (~10-20ms) because the packages are already bundled
        into the factory's snapshot.

        Args:
            site_packages: Optional path to additional site-packages.
                If not provided, uses the site-packages from initialization.
            resource_limits: Optional resource limits for the sandbox.
            network: Optional network configuration. If provided, enables networking.
            callbacks: Optional callbacks that sandboxed code can invoke.
                Can be a CallbackRegistry or a list of callback dicts.
            secrets: Optional dict of secret names to secret configurations.
                Each value is a dict with "value" (required) and "allowed_hosts"
                (optional, defaults to []).
            scrub_stdout: Whether to scrub secret placeholders from stdout.
                Defaults to True when secrets are provided.
            scrub_stderr: Whether to scrub secret placeholders from stderr.
                Defaults to True when secrets are provided.
            scrub_files: Whether to scrub secret placeholders from file writes.
                Defaults to True when secrets are provided.

        Returns:
            A new Sandbox ready to execute Python code.

        Raises:
            InitializationError: If sandbox creation fails.

        Example:
            sandbox = factory.create_sandbox()
            result = sandbox.execute('print("Hello!")')

            # With secrets
            sandbox = factory.create_sandbox(
                secrets={"API_KEY": {"value": "sk-xxx", "allowed_hosts": ["api.example.com"]}},
                network=NetConfig(allowed_hosts=["api.example.com"]),
            )
        """
        ...

    def to_bytes(self) -> bytes:
        """Get the pre-compiled runtime as bytes.

        This can be used for custom serialization or inspection.
        """
        ...


class Session:
    """A session that maintains persistent Python state across executions.

    Unlike `Sandbox` which runs each execution in isolation, `Session` preserves
    Python variables, functions, and classes between `execute()` calls. This is
    useful for:

    - Interactive REPL-style execution
    - Building up state incrementally
    - Faster subsequent executions (no Python initialization overhead)

    Sessions can optionally use a virtual filesystem (VFS) for persistent file
    storage that survives across executions and even session resets.

    Example:
        # Basic session usage
        session = Session()
        session.execute('x = 1')
        session.execute('y = 2')
        result = session.execute('print(x + y)')
        print(result.stdout)  # "3"

        # Session with VFS for file persistence
        storage = VfsStorage()
        session = Session(vfs=storage)
        session.execute('open("/data/test.txt", "w").write("hello")')
        result = session.execute('print(open("/data/test.txt").read())')
        print(result.stdout)  # "hello"

        # Session with callbacks
        def get_time():
            import time
            return {"timestamp": time.time()}

        session = Session(callbacks=[
            {"name": "get_time", "fn": get_time, "description": "Returns current time"}
        ])
    """

    @property
    def execution_count(self) -> int:
        """Number of executions performed in this session."""
        ...

    @property
    def execution_timeout_ms(self) -> Optional[int]:
        """Current execution timeout in milliseconds, or None if not set."""
        ...

    @execution_timeout_ms.setter
    def execution_timeout_ms(self, value: Optional[int]) -> None:
        """Set the execution timeout in milliseconds."""
        ...

    @property
    def vfs(self) -> Optional[VfsStorage]:
        """VFS storage used by this session, if any."""
        ...

    @property
    def vfs_mount_path(self) -> Optional[str]:
        """VFS mount path, if VFS is enabled."""
        ...

    def __init__(
        self,
        *,
        vfs: Optional[VfsStorage] = None,
        vfs_mount_path: Optional[str] = None,
        execution_timeout_ms: Optional[int] = None,
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        volumes: Optional[Sequence[tuple[str, str, bool]]] = None,
        on_stdout: Optional[Callable[[str], None]] = None,
        on_stderr: Optional[Callable[[str], None]] = None,
    ) -> None:
        """Create a new session with the embedded Python runtime.

        Sessions maintain persistent Python state across `execute()` calls,
        unlike `Sandbox` which runs each execution in isolation.

        Args:
            vfs: Optional VfsStorage for persistent file storage.
                Files written to `/data/*` will persist across executions.
            vfs_mount_path: Custom mount path for VFS (default: "/data").
            execution_timeout_ms: Optional timeout in milliseconds for each execution.
            network: Optional network configuration. If provided, enables networking.
            callbacks: Optional callbacks that sandboxed code can invoke.
                Can be a CallbackRegistry or a list of callback dicts.
            mcp: Optional MCPManager with connected MCP servers.
            on_stdout: Optional callback for streaming stdout output in real-time.
            on_stderr: Optional callback for streaming stderr output in real-time.

        Raises:
            InitializationError: If the session fails to initialize.

        Example:
            # Basic session
            session = Session()
            session.execute('x = 42')
            result = session.execute('print(x)')  # prints "42"

            # Session with MCP tools
            manager = MCPManager()
            manager.connect("fs", "npx", ["-y", "@anthropic/mcp-server-filesystem", "."])
            session = Session(mcp=manager)

            # Session with callbacks
            def echo(message: str):
                return {"echoed": message}

            session = Session(callbacks=[
                {"name": "echo", "fn": echo, "description": "Echoes the message"}
            ])
        """
        ...

    def execute(self, code: str) -> ExecuteResult:
        """Execute Python code in the session.

        Unlike `Sandbox.execute()`, state from previous executions is preserved.
        Variables, functions, and classes defined in one call are available in
        subsequent calls.

        Args:
            code: Python source code to execute.

        Returns:
            ExecuteResult containing stdout and execution statistics.

        Raises:
            ExecutionError: If the Python code raises an exception.
            TimeoutError: If execution exceeds the timeout limit.

        Example:
            session.execute('x = 1')
            session.execute('y = 2')
            result = session.execute('print(x + y)')
            print(result.stdout)  # "3"
        """
        ...

    def reset(self) -> None:
        """Reset the session to a fresh state.

        This clears all Python variables and state, but VFS storage persists
        if it was provided at session creation.

        Example:
            session.execute('x = 42')
            session.reset()
            # x is no longer defined
            session.execute('print(x)')  # raises NameError
        """
        ...

    def clear_state(self) -> None:
        """Clear Python state without fully resetting the session.

        This is lighter-weight than `reset()` - it clears Python variables
        but doesn't recreate the WASM instance.

        Example:
            session.execute('x = 42')
            session.clear_state()
            # x is no longer defined
        """
        ...

    def snapshot_state(self) -> bytes:
        """Capture a snapshot of the current Python state.

        The snapshot contains all user-defined variables, serialized using pickle.
        It can be saved to disk and restored later.

        Returns:
            The serialized snapshot data as bytes.

        Raises:
            ExecutionError: If the state cannot be serialized.

        Example:
            session.execute('x = 42')
            snapshot = session.snapshot_state()
            # Save snapshot to file
            with open('state.bin', 'wb') as f:
                f.write(snapshot)
        """
        ...

    def restore_state(self, snapshot: bytes) -> None:
        """Restore Python state from a previously captured snapshot.

        Args:
            snapshot: The serialized snapshot data (bytes).

        Raises:
            ExecutionError: If the snapshot cannot be restored.

        Example:
            # Load snapshot from file
            with open('state.bin', 'rb') as f:
                snapshot = f.read()
            session.restore_state(snapshot)
            result = session.execute('print(x)')  # x was in the snapshot
        """
        ...


class VfsStorage:
    """In-memory virtual filesystem storage.

    VfsStorage provides persistent file storage for sessions. Files written
    to the VFS mount path (default: `/data/*`) are stored in memory and
    persist across executions and session resets.

    The same VfsStorage instance can be shared across multiple sessions
    to allow file sharing between them.

    Example:
        storage = VfsStorage()
        session = Session(vfs=storage)
        session.execute('open("/data/test.txt", "w").write("hello")')

        # Create another session with the same storage
        session2 = Session(vfs=storage)
        result = session2.execute('print(open("/data/test.txt").read())')
        print(result.stdout)  # "hello"
    """

    def __init__(self) -> None:
        """Create a new empty VFS storage."""
        ...


__version__: str
"""Version of the pyeryx package."""
