"""Type stubs for the eryx native module."""

import asyncio
import builtins
from pathlib import Path
from typing import Any, Callable, Iterator, Optional, Sequence, TypedDict, Union, final

PathLike = Union[str, Path]

VolumeSpec = Union[tuple[str, str, bool], tuple[str, str, bool, Optional[int]]]
"""A host volume mount: `(host_path, guest_path, read_only)`, optionally with a
fourth `max_file_size` in bytes. Writes that would grow any file in the mount
past it fail. Without it, writes to a writable volume are unmetered.

Never mount a directory holding a factory file writable: the sandbox's writes
change the host file in place, including native code a loaded factory runs."""


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


@final
class MCPManager:
    """Manager for MCP (Model Context Protocol) server connections.

    Creates connections to MCP servers and exposes their tools as native
    Rust callbacks that can be used by ``Sandbox`` and ``Session``.

    Tools are exposed as callbacks under ``mcp["<server>"].<tool>``,
    so sandboxed code can call them as ``await mcp["server"].tool(args)``.

    Example:
        manager = MCPManager()
        manager.connect("fs", "npx", ["-y", "@anthropic/mcp-server-filesystem", "."])
        tools = manager.list_tools()
        sandbox = Sandbox(mcp=manager)
        result = sandbox.execute('data = await mcp["fs"].read_file(path="README.md")')
    """

    @property
    def server_names(self) -> list[str]:
        """Names of all connected MCP servers."""
        ...

    def __new__(cls) -> MCPManager:
        """Create a new empty MCP manager."""
        ...

    def connect(
        self,
        name: str,
        command: str,
        args: list[str] = ...,
        env: dict[str, str] = ...,
        timeout_secs: float = 30.0,
    ) -> int:
        """Connect to an MCP server by spawning a child process.

        Args:
            name: A human-readable name for this server connection.
            command: The command to spawn (e.g., "npx", "uvx").
            args: Arguments to pass to the command.
            env: Environment variables to set for the child process.
            timeout_secs: Timeout in seconds for the connection handshake and
                the initial tool listing.

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


@final
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

    def __new__(cls) -> CallbackRegistry:
        """Create a new empty callback registry."""
        ...

    def callback(
        self,
        name: Optional[str] = None,
        description: Optional[str] = None,
        schema: Optional[dict[str, Any]] = None,
    ) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """Decorator to register a callback function.

        Registering a name that is already registered replaces the earlier
        callback, as with `add`.

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
        callable: Callable[..., Any],
        *,
        name: Optional[str] = None,
        description: Optional[str] = None,
        schema: Optional[dict[str, Any]] = None,
    ) -> None:
        """Add a callback directly without using the decorator pattern.

        Like a dict, adding a name that is already registered replaces the
        earlier callback (e.g. when a notebook cell is re-run).

        Args:
            callable: The callable to register.
            name: Optional name. Defaults to callable.__name__.
            description: Optional description. Defaults to callable.__doc__ or empty string.
            schema: Optional JSON Schema dict.
        """
        ...

    def __len__(self) -> int:
        """Return the number of registered callbacks."""
        ...

    def __iter__(self) -> Iterator[CallbackDict]:
        """Return an iterator over the registered callbacks as dicts."""
        ...


@final
class ExecuteResult:
    """Result of executing Python code in the sandbox."""

    @property
    def stdout(self) -> bytes:
        """Complete stdout output from the sandboxed code as raw bytes."""
        ...

    @property
    def stderr(self) -> bytes:
        """Complete stderr output from the sandboxed code as raw bytes."""
        ...

    @property
    def stdout_text(self) -> str:
        """Stdout decoded as UTF-8 with replacement characters for invalid bytes."""
        ...

    @property
    def stderr_text(self) -> str:
        """Stderr decoded as UTF-8 with replacement characters for invalid bytes."""
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

    @property
    def fuel_consumed(self) -> Optional[int]:
        """Fuel (WASM instructions) consumed during execution (if available)."""
        ...

    @property
    def result(self) -> Any:
        """The script's ``result`` variable, parsed from JSON into a native Python
        value, or ``None`` if it was not set.

        The captured variable name defaults to ``result`` and can be changed via the
        ``result_variable`` argument to ``Sandbox``/``Session``. The variable is
        consumed after each execution, so in a ``Session`` a later run that does not
        set it reports ``None`` rather than the previous value. If the value was set
        but could not be JSON-serialized, this is ``None`` and ``result_error``
        explains why."""
        ...

    @property
    def result_json(self) -> Optional[str]:
        """The raw JSON string of the captured ``result`` variable, or ``None``."""
        ...

    @property
    def result_error(self) -> Optional[str]:
        """Why result capture failed (e.g. the value was not JSON-serializable), or
        ``None`` when capture succeeded or no result variable was set."""
        ...

    @property
    def state_was_reset(self) -> bool:
        """Whether a ``Session`` lost its Python state before this run.

        An earlier call was cancelled or halted with no snapshot to roll back
        to, so the session was reset: variables from before that call are gone
        and the preamble ran again. Reported once, by the first successful run
        after the reset. Always ``False`` for ``Sandbox.execute()``."""
        ...


@final
class SuspendedCallback:
    """Details of the callback that suspended execution by raising ``SuspendCallback``."""

    @property
    def name(self) -> str:
        """Name of the callback that suspended."""
        ...

    @property
    def args_json(self) -> str:
        """Canonicalized JSON arguments the callback was invoked with."""
        ...

    @property
    def reason(self) -> str:
        """The reason string passed to ``SuspendCallback``."""
        ...


@final
class StateSnapshot:
    """A captured session state, returned by ``Session.snapshot_state()``."""

    @property
    def data(self) -> bytes:
        """The serialized state, as accepted by ``Session.restore_state()``."""
        ...

    @property
    def skipped_globals(self) -> list[str]:
        """Names of the globals left out because they could not be serialized.

        These (e.g. open sockets, generators) raise ``NameError`` after the
        snapshot is restored.
        """
        ...


@final
class ReplayOutcome:
    """Result of ``Sandbox.execute_with_journal()``.

    Exactly one of ``result`` and ``error`` is set. ``journal`` is always present,
    even when execution failed or suspended. When ``suspended`` is set, ``error``
    holds the resulting ``ExecutionError``; branch on ``suspended`` first.
    """

    @property
    def result(self) -> Optional[ExecuteResult]:
        """The execution result, or ``None`` if execution failed or suspended."""
        ...

    @property
    def error(self) -> Optional[EryxError]:
        """The exception ``execute()`` would have raised, or ``None`` on success."""
        ...

    @property
    def journal(self) -> dict[str, Any]:
        """The callback journal recorded during this run, as a JSON-compatible dict.

        Persist it with ``json.dumps`` and pass it back as ``journal=`` to a later
        ``execute_with_journal()``. Treat it as opaque and as a trusted input."""
        ...

    @property
    def replayed_callbacks(self) -> int:
        """How many callbacks were served from the replay journal."""
        ...

    @property
    def suspended(self) -> Optional[SuspendedCallback]:
        """The callback that suspended execution, or ``None``."""
        ...


@final
class NetConfig:
    """Network configuration for sandbox execution.

    Use this class to configure which hosts Python code can connect to,
    set timeouts, and add custom certificates.

    By default, network access is **disabled**. To enable networking,
    create a `NetConfig` and pass it to the Sandbox. A default `NetConfig()`
    allows no hosts: list them in `allowed_hosts` (or with `allow_host()`),
    or set `allow_all_hosts=True` to allow every non-blocked host.

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

    allow_all_hosts: bool
    """When true, all non-blocked hosts are allowed (allowed_hosts is ignored)."""

    allowed_hosts: list[str]
    """Allowed host patterns (only checked when allow_all_hosts is false).

    Assigning this turns allow_all_hosts off.
    """

    blocked_hosts: list[str]
    """Blocked host patterns."""

    blocked_networks: list[str]
    """CIDRs that a host's resolved addresses must not fall in.

    Checked after DNS resolution, so a public name pointing at an internal
    address is refused. Assigning an invalid CIDR raises ValueError.
    """

    allowed_networks: list[str]
    """CIDRs exempt from blocked_networks."""

    internal_hosts: list[str]
    """Host patterns allowed to resolve into blocked_networks.

    For trusted names routed internally on purpose (e.g. via cluster DNS).
    Only list names whose DNS you control.
    """

    allow_plaintext_secrets: bool
    """Also substitute secret placeholders on plain TCP connections.

    Off by default: placeholders are only replaced after a TLS upgrade, so a
    request over ``http://`` goes out with the placeholder left in place.
    Turning this on sends the real secret value unencrypted, where anyone on
    the network path can read it. Only enable it for trusted local test servers.
    """

    def __new__(
        cls,
        *,
        max_connections: int = 10,
        connect_timeout_ms: int = 30000,
        io_timeout_ms: int = 60000,
        allow_all_hosts: bool = False,
        allowed_hosts: Optional[Sequence[str]] = None,
        blocked_hosts: Optional[Sequence[str]] = None,
        blocked_networks: Optional[Sequence[str]] = None,
        allowed_networks: Optional[Sequence[str]] = None,
        internal_hosts: Optional[Sequence[str]] = None,
        allow_plaintext_secrets: bool = False,
    ) -> NetConfig:
        """Create new network configuration.

        By default:
        - max_connections: 10
        - connect_timeout_ms: 30000 (30 seconds)
        - io_timeout_ms: 60000 (60 seconds)
        - allow_all_hosts: False (only allowed_hosts may be reached)
        - allowed_hosts: [] (no hosts allowed)
        - blocked_hosts: localhost and private networks
        - blocked_networks: every non-public range (loopback, private,
          link-local/cloud metadata, CGNAT, multicast, ...)
        - allow_plaintext_secrets: False (secrets substituted only over TLS)

        Args:
            max_connections: Maximum number of concurrent connections.
            connect_timeout_ms: Timeout for establishing connections. Must be greater than zero.
            io_timeout_ms: Timeout for read/write operations. Must be greater than zero.
            allow_all_hosts: When true, all non-blocked hosts are allowed
                (allowed_hosts is ignored).
            allowed_hosts: List of allowed host patterns (supports wildcards like "*.example.com").
            blocked_hosts: List of blocked host patterns.
            blocked_networks: CIDRs (e.g. "10.0.0.0/8") that a host's resolved
                addresses must not fall in.
            allowed_networks: CIDRs exempt from blocked_networks.
            internal_hosts: Host patterns allowed to resolve into blocked_networks.
            allow_plaintext_secrets: Also substitute secret placeholders on plain
                TCP (e.g. ``http://``), not only after a TLS upgrade. This sends
                the real secret value unencrypted; only enable it for trusted
                local test servers.

        Raises:
            ValueError: If a network is not a valid CIDR.

        Example:
            # Only allow specific APIs
            net = NetConfig(allowed_hosts=["api.example.com", "*.openai.com"])

            # Any public host, with the default blocks
            net = NetConfig(allow_all_hosts=True)
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

        Patterns support wildcards: `*.example.com`, `api.*.com`.
        Turns allow_all_hosts off, so only the allowed hosts can be reached.

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

    def allow_network(self, cidr: str) -> NetConfig:
        """Allow addresses in a CIDR even though they fall in blocked_networks.

        Returns self for method chaining.

        Raises:
            ValueError: If `cidr` is not a valid CIDR.

        Example:
            net = NetConfig(allowed_hosts=["svc.cluster.local"]).allow_network("10.96.0.0/12")
        """
        ...

    def allow_internal_host(self, pattern: str) -> NetConfig:
        """Let hosts matching a pattern resolve to internal addresses.

        For trusted names routed internally on purpose (e.g. a service reached
        through cluster DNS). Only list names whose DNS you control.

        Returns self for method chaining.

        Example:
            net = NetConfig(allowed_hosts=["api.internal.example.com"]).allow_internal_host(
                "api.internal.example.com"
            )
        """
        ...

    def allow_localhost(self) -> NetConfig:
        """Allow connections to localhost (disabled by default).

        Returns self for method chaining.

        Example:
            net = NetConfig(allowed_hosts=["localhost"]).allow_localhost()
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


@final
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

    max_fuel: Optional[int]
    """Maximum fuel (WASM instructions) allowed."""

    max_vfs_bytes: Optional[int]
    """Maximum total bytes the in-memory virtual filesystem may hold.

    VFS files live in host memory, outside `max_memory_bytes`, and the script
    picks its own write offsets. `None` uses the default (64 MB).
    """

    max_open_host_files: Optional[int]
    """Maximum host files (stdlib, site-packages and volume files) a script
    may hold open at once.

    Opens past it fail with `EDQUOT`. `None` uses the default (256).
    """

    def __new__(
        cls,
        *,
        execution_timeout_ms: Optional[int] = 30000,
        callback_timeout_ms: Optional[int] = 10000,
        max_memory_bytes: Optional[int] = 134217728,
        max_callback_invocations: Optional[int] = 1000,
        max_fuel: Optional[int] = None,
        max_vfs_bytes: Optional[int] = None,
        max_open_host_files: Optional[int] = None,
    ) -> ResourceLimits:
        """Create new resource limits.

        All parameters are optional. If not specified, defaults are used:
        - execution_timeout_ms: 30000 (30 seconds)
        - callback_timeout_ms: 10000 (10 seconds)
        - max_memory_bytes: 134217728 (128 MB)
        - max_callback_invocations: 1000
        - max_fuel: None (unlimited)
        - max_vfs_bytes: None (64 MB)
        - max_open_host_files: None (256)

        Pass `None` to disable a specific limit. `max_vfs_bytes` and
        `max_open_host_files` are the exceptions: `None` means the default
        cap, since both bound host resources.
        """
        ...

    @staticmethod
    def unlimited() -> ResourceLimits:
        """Create resource limits with no restrictions.

        Warning: Use with caution! Code can run indefinitely and use unlimited memory.

        The virtual filesystem cap still applies (see `max_vfs_bytes`): it bounds
        *host* memory, so removing it would let a script kill the process.
        """
        ...


@final
class Sandbox:
    """A Python sandbox powered by WebAssembly.

    The Sandbox executes Python code in complete isolation from the host system.
    Each sandbox has its own memory space and cannot access files, network,
    or other system resources unless explicitly provided via callbacks.

    This creates a fast sandbox (~1-5ms) using the pre-initialized Python runtime.
    The sandbox has access to Python's standard library but no third-party packages.

    For sandboxes with custom packages, use `SandboxFactory` instead.

    Execution trace collection (`sys.settrace`) is disabled for Python
    sandboxes; `ExecuteResult` does not expose trace events, so there is no
    per-line tracing overhead.

    Example:
        # Basic sandbox (stdlib only)
        sandbox = Sandbox()
        result = sandbox.execute('print("Hello from the sandbox!")')
        print(result.stdout_text)  # "Hello from the sandbox!"

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

    def __new__(
        cls,
        *,
        resource_limits: Optional[ResourceLimits] = None,
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        secrets: Optional[dict[str, SecretDict]] = None,
        scrub_stdout: Optional[bool] = None,
        scrub_stderr: Optional[bool] = None,
        scrub_result: Optional[bool] = None,
        scrub_files: Optional[bool] = None,
        volumes: Optional[Sequence[VolumeSpec]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
        result_variable: Optional[str] = None,
    ) -> Sandbox:
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
            scrub_result: Whether to scrub secret placeholders from the structured
                ``result`` (and ``result_error``). Defaults to False even when secrets
                are provided: the result is a programmatic side channel, so scrubbing
                is opt-in.
            scrub_files: Whether to scrub secret placeholders from file writes.
                Defaults to True when secrets are provided.
            on_stdout: Optional callback for streaming stdout output.
                Called in real-time as Python code writes to stdout, rather than
                waiting for execution to complete. Receives the output chunk as bytes.
            on_stderr: Optional callback for streaming stderr output.
                Called in real-time as Python code writes to stderr, rather than
                waiting for execution to complete. Receives the output chunk as bytes.
            result_variable: Name of the variable captured from the script and exposed
                as ``ExecuteResult.result`` (JSON-serialized). Defaults to ``"result"``.

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

    def execute(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> ExecuteResult:
        """Execute Python code in the sandbox.

        The code runs in complete isolation. Any output to stdout is captured
        and returned in the result.

        The GIL is released while the code runs. Ctrl-C (or any signal handler
        that raises) stops the run and re-raises the handler's exception, e.g.
        ``KeyboardInterrupt``.

        Args:
            code: Python source code to execute.
            cancel: Optional token; calling ``cancel.cancel()`` from any thread
                stops the run.

        Returns:
            ExecuteResult containing stdout, timing info, and statistics.

        Raises:
            ExecutionError: If the Python code raises an exception.
            TimeoutError: If execution exceeds the timeout limit.
            ResourceLimitError: If a resource limit is exceeded.
            CancelledError: If ``cancel`` was cancelled.

        Example:
            result = sandbox.execute('''
            x = 2 + 2
            print(f"2 + 2 = {x}")
            ''')
            print(result.stdout)  # "2 + 2 = 4\\n"
        """
        ...

    def execute_async(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> asyncio.Future[ExecuteResult]:
        """Execute Python code in the sandbox from asyncio.

        Like ``execute()``, but returns an ``asyncio.Future``, so the event loop
        keeps running while the code executes. Cancelling the future (or the
        task awaiting it) stops the run. It is not a coroutine: wrap it with
        ``asyncio.ensure_future()`` rather than ``asyncio.create_task()``.

        Args:
            code: Python source code to execute.
            cancel: Optional token; cancelling it stops the run and raises
                ``CancelledError``.

        Example:
            result = await sandbox.execute_async('print("hi")')
        """
        ...

    def execute_with_journal(
        self, code: str, journal: Optional[dict[str, Any]] = None
    ) -> ReplayOutcome:
        """Execute code, journaling callback results and replaying them from
        ``journal`` if one was given.

        Unlike ``execute()``, this does not raise for execution failures: the error
        is returned on the outcome alongside the journal, which is always populated.

        Args:
            code: Python source code to execute.
            journal: A journal from a previous ``ReplayOutcome.journal``. Matching
                callbacks return their recorded results instead of running live.
                Trusted input: replayed results reach the sandbox verbatim.

        Raises:
            ValueError: If ``journal`` is not a valid journal.

        Example:
            outcome = sandbox.execute_with_journal(code)
            if outcome.suspended:
                save(outcome.journal)  # resume later with journal=load()
            elif outcome.error:
                raise outcome.error
        """
        ...


class SuspendCallback(Exception):
    """Raise from a callback to suspend execution.

    The argument is an opaque reason string, surfaced as
    ``ReplayOutcome.suspended.reason``. The guest halts immediately and the
    suspended call is not journaled, so it runs live again on resume.
    """

    ...


class EryxError(Exception):
    """Base exception for all Eryx errors."""

    ...


class ExecutionError(EryxError):
    """Error during Python code execution in the sandbox."""

    ...


class PythonExceptionError(ExecutionError):
    """The sandboxed Python code raised an uncaught exception.

    A subclass of ExecutionError, so ``except ExecutionError`` still catches
    it; catch this instead to tell a script error apart from a sandbox failure.
    """

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


@final
class CancelToken:
    """A token for cancelling executions from any thread.

    Pass it as ``cancel=`` to ``execute()`` or ``execute_async()``; calling
    ``cancel()`` stops the run, which then raises ``CancelledError``. A token
    stays cancelled, so later executions given the same token are cancelled
    before they start. One token may be shared by several executions.

    Example:
        token = CancelToken()
        threading.Timer(1.0, token.cancel).start()
        try:
            sandbox.execute("while True: pass", cancel=token)
        except CancelledError:
            print("stopped")
    """

    def __init__(self) -> None: ...
    def cancel(self) -> None:
        """Cancel every execution using this token. Safe to call from any
        thread, and more than once."""
        ...

    @property
    def cancelled(self) -> bool:
        """Whether ``cancel()`` has been called."""
        ...


class CancelledError(EryxError):
    """Execution was cancelled."""

    ...


class PoolError(EryxError):
    """Base exception for pool operations."""

    ...


class PoolTimeoutError(PoolError):
    """Timed out waiting for an available sandbox."""

    ...


class PoolClosedError(PoolError):
    """Pool has been closed."""

    ...


@final
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

    def __new__(
        cls,
        *,
        site_packages: Optional[PathLike] = None,
        packages: Optional[Sequence[PathLike]] = None,
        imports: Optional[Sequence[str]] = None,
        setup_code: Optional[str] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        cache: bool = True,
    ) -> SandboxFactory:
        """Create a new sandbox factory with custom packages.

        This performs one-time initialization that can take 3-5 seconds,
        but subsequent sandbox creation will be very fast.

        Args:
            site_packages: Optional path to a directory containing Python packages.
            packages: Optional list of paths to .whl or .tar.gz package files.
                These are extracted and their native extensions are linked.
            imports: Optional list of module names to pre-import during initialization.
                Pre-imported modules are immediately available without import overhead.
            setup_code: Optional Python code to execute after imports, baked into the
                snapshot. Use this to pre-create objects (e.g., a Jinja2
                ``SandboxedEnvironment``) so every sandbox starts with them already
                in memory. Each sandbox gets its own copy-on-write clone, preserving
                full isolation.
            callbacks: Optional callbacks (a ``CallbackRegistry`` or a list of callback
                dicts) whose declarations are baked into the snapshot, so sandboxes
                created from this factory skip the per-sandbox callback setup (a few
                milliseconds per sandbox). ``create_sandbox()`` and ``create_session()``
                register these callbacks unless given their own; a sandbox given a
                different set still works, it just installs them itself. Setup code
                cannot invoke the callbacks.
            cache: Whether to cache the pre-compiled component in the process-global
                cache. When enabled, a BLAKE3 content hash is computed once during
                factory construction and subsequent ``create_sandbox()`` calls skip
                component deserialization entirely (~0.8ms vs ~8ms per call).
                Defaults to True.

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

            # With setup code baked into the snapshot
            factory = SandboxFactory(
                packages=["jinja2.whl", "markupsafe.whl"],
                imports=["jinja2", "jinja2.sandbox"],
                setup_code=(
                    "from jinja2.sandbox import SandboxedEnvironment\\n"
                    "env = SandboxedEnvironment()\\n"
                ),
            )
        """
        ...

    @staticmethod
    def load(
        path: PathLike,
        *,
        site_packages: Optional[PathLike] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        cache: bool = True,
    ) -> SandboxFactory:
        """Load a sandbox factory from a file.

        This loads a previously saved factory, which is much faster than
        creating a new one (~10ms vs ~3-5s).

        The file contains pre-compiled native code that is loaded without
        validation, so only load factory files you created yourself (with
        ``save()``) and that nobody else can modify. On Unix, files writable by
        other users, or owned by a user other than you or root, are refused.

        The pre-compiled component is memory-mapped rather than read into
        memory, so processes loading the same file share its pages (except on
        Windows, which can't replace a mapped file, so it is read there). Don't
        modify a factory file in place while it is loaded; ``save()`` replaces
        it atomically, which is safe. For the same reason, never keep a factory
        file in a directory mounted into a sandbox as a writable volume.

        Args:
            path: Path to the saved factory file.
            site_packages: Optional path to site-packages directory.
                Required if the factory was saved without embedded packages.
            callbacks: The callbacks the factory was created with, if any. The file
                holds their baked declarations but not the Python callables, so pass
                the same callbacks here to keep the setup-free fast path.
            cache: Whether to cache the pre-compiled component in the process-global
                cache. When enabled, a BLAKE3 content hash is computed once during
                loading and subsequent ``create_sandbox()`` calls reuse the linked
                component instead of re-linking it per call. Defaults to True.

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

    def create_session(
        self,
        *,
        vfs: Optional[VfsStorage] = None,
        vfs_mount_path: Optional[str] = None,
        resource_limits: Optional[ResourceLimits] = None,
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        volumes: Optional[Sequence[VolumeSpec]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
        result_variable: Optional[str] = None,
    ) -> Session:
        """Create a persistent session from this factory's preinitialized runtime.

        The session preserves interpreter and module state across executions,
        while remaining isolated from other sessions and sandboxes. The factory
        can be dropped after creation; extracted package storage remains owned
        by the session.

        `vfs_mount_path` applies only when caller-provided `vfs` storage or
        `volumes` enable a wrapper-visible VFS; by itself it does not expose
        `Session.vfs`. Caller-provided `vfs` retains its own policy and quota.

        Supported options include VFS storage and mounts, resource limits,
        networking, callbacks, MCP tools, volumes, output handlers, and a result
        variable.
        `resource_limits` defaults to `ResourceLimits()` (30-second execution
        timeout, 128 MB memory, 10-second callback timeout, 1000 callback
        invocations).

        Returns a persistent `Session`. Raises `InitializationError` if it
        cannot be initialized.
        """
        ...

    def create_sandbox(
        self,
        *,
        site_packages: Optional[PathLike] = None,
        resource_limits: Optional[ResourceLimits] = None,
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        secrets: Optional[dict[str, SecretDict]] = None,
        scrub_stdout: Optional[bool] = None,
        scrub_stderr: Optional[bool] = None,
        scrub_result: Optional[bool] = None,
        scrub_files: Optional[bool] = None,
        volumes: Optional[Sequence[VolumeSpec]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
        result_variable: Optional[str] = None,
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
            mcp: Optional MCPManager with connected MCP servers. Tools from
                connected servers are exposed as callbacks named
                ``mcp.<server>.<tool>``.
            secrets: Optional dict of secret names to secret configurations.
                Each value is a dict with "value" (required) and "allowed_hosts"
                (optional, defaults to []).
            scrub_stdout: Whether to scrub secret placeholders from stdout.
                Defaults to True when secrets are provided.
            scrub_stderr: Whether to scrub secret placeholders from stderr.
                Defaults to True when secrets are provided.
            scrub_result: Whether to scrub secret placeholders from the structured
                ``result`` (and ``result_error``). Defaults to False even when
                secrets are provided.
            scrub_files: Whether to scrub secret placeholders from file writes.
                Defaults to True when secrets are provided.
            result_variable: Name of the variable captured from the script and exposed
                as ``ExecuteResult.result`` (JSON-serialized). Defaults to ``"result"``.

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

    def create_pool(
        self,
        *,
        max_size: int = 10,
        min_idle: int = 1,
        acquire_timeout_ms: int = 30000,
        idle_timeout_ms: int = 300000,
        network: Optional[NetConfig] = None,
        secrets: Optional[dict[str, SecretDict]] = None,
        scrub_stdout: Optional[bool] = None,
        scrub_stderr: Optional[bool] = None,
        scrub_files: Optional[bool] = None,
    ) -> SandboxPool:
        """Create a sandbox pool from this factory.

        The pool reuses the factory's precompiled artifact, so when the idle
        queue is empty and a new sandbox must be created, it skips component
        deserialization. Per-request state (callbacks, output handlers, resource
        limits) is set on each ``acquire()`` call and cleared on release.

        Args:
            max_size: Maximum concurrent sandboxes. Default: 10.
            min_idle: Minimum warm sandboxes to keep ready. Default: 1.
            acquire_timeout_ms: Max wait for a sandbox in ms. Default: 30000.
            idle_timeout_ms: Idle sandbox eviction threshold in ms. Default: 300000.
            network: Optional network configuration for every pooled sandbox.
            secrets: Optional secrets for every pooled sandbox, in the same
                format as ``Sandbox(secrets=...)``.
            scrub_stdout: Whether to scrub secret placeholders from stdout.
                Defaults to True when secrets are provided.
            scrub_stderr: Whether to scrub secret placeholders from stderr.
                Defaults to True when secrets are provided.
            scrub_files: Whether to scrub secret placeholders from file writes.
                Defaults to True when secrets are provided.

        Network and secrets are fixed for the pool's lifetime, unlike the
        per-request settings passed to ``acquire()``.

        Returns:
            A ``SandboxPool`` ready to lease sandboxes.

        Raises:
            InitializationError: If pool creation or pre-warming fails.

        Example:
            factory = SandboxFactory(imports=["json"], cache=True)
            pool = factory.create_pool(max_size=4, min_idle=1)
            with pool.acquire() as sandbox:
                result = sandbox.execute('import json; print(json.dumps([1]))')
            pool.close()
        """
        ...

    def to_bytes(self) -> bytes:
        """Get the pre-compiled runtime as bytes.

        This can be used for custom serialization or inspection.
        """
        ...


@final
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

    `snapshot_state()`, `restore_state()` and `clear_state()` run under the
    execution timeout raised to at least 5 seconds, or 30 seconds when the
    session has no execution timeout. That state-operation bound can't be set
    separately from Python yet.

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
    def fuel_limit(self) -> Optional[int]:
        """Current fuel limit (max WASM instructions per execution), or None if not set."""
        ...

    @fuel_limit.setter
    def fuel_limit(self, value: Optional[int]) -> None:
        """Set the fuel limit, or None to disable it (fuel is still tracked)."""
        ...

    @property
    def vfs(self) -> Optional[VfsStorage]:
        """VFS storage used by this session, if any."""
        ...

    @property
    def vfs_mount_path(self) -> Optional[str]:
        """VFS mount path, if VFS is enabled."""
        ...

    def __new__(
        cls,
        *,
        vfs: Optional[VfsStorage] = None,
        vfs_mount_path: Optional[str] = None,
        resource_limits: Optional[ResourceLimits] = None,
        execution_timeout_ms: Optional[int] = None,
        max_fuel: Optional[int] = None,
        network: Optional[NetConfig] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        mcp: Optional[MCPManager] = None,
        volumes: Optional[Sequence[VolumeSpec]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
        result_variable: Optional[str] = None,
    ) -> Session:
        """Create a new session with the embedded Python runtime.

        Sessions maintain persistent Python state across `execute()` calls,
        unlike `Sandbox` which runs each execution in isolation.

        Args:
            vfs: Optional VfsStorage for persistent file storage.
                Files written to `/data/*` will persist across executions.
            vfs_mount_path: Custom mount path for VFS (default: "/data").
            resource_limits: Optional limits for each execution. Defaults to
                `ResourceLimits()` (30-second execution timeout, 128 MB memory).
            execution_timeout_ms: Overrides `resource_limits.execution_timeout_ms`.
            max_fuel: Overrides `resource_limits.max_fuel` (max WASM instructions).
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

    def execute(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> ExecuteResult:
        """Execute Python code in the session.

        Unlike `Sandbox.execute()`, state from previous executions is preserved.
        Variables, functions, and classes defined in one call are available in
        subsequent calls.

        The GIL is released while the code runs. Ctrl-C (or any signal handler
        that raises) stops the run and re-raises the handler's exception.

        A cancelled or interrupted run loses the session's Python state: the
        next call starts from a fresh interpreter. VFS files are kept.

        Args:
            code: Python source code to execute.
            cancel: Optional token; calling ``cancel.cancel()`` from any thread
                stops the run.

        Returns:
            ExecuteResult containing stdout and execution statistics.

        Raises:
            ExecutionError: If the Python code raises an exception.
            TimeoutError: If execution exceeds the timeout limit.
            CancelledError: If ``cancel`` was cancelled.

        Example:
            session.execute('x = 1')
            session.execute('y = 2')
            result = session.execute('print(x + y)')
            print(result.stdout)  # "3"
        """
        ...

    def execute_async(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> asyncio.Future[ExecuteResult]:
        """Execute code in the session from asyncio.

        Like ``execute()``, but returns an awaitable, so the event loop keeps
        running while the code executes. Concurrent calls on one session run
        one at a time, in call order, and ``reset()``, ``clear_state()``,
        ``snapshot_state()`` and ``restore_state()`` wait their turn behind
        calls made before them. Cancelling the awaiting task stops the run,
        losing session state as with ``cancel``. Raises ``RuntimeError``
        without running anything when called outside a running event loop.

        Example:
            await session.execute_async('x = 1')
            result = await session.execute_async('print(x)')
        """
        ...

    def execute_with_journal(
        self, code: str, journal: Optional[dict[str, Any]] = None
    ) -> ReplayOutcome:
        """Execute code in the session with callback journaling and replay.

        Session state persists as with ``execute()``. See
        ``Sandbox.execute_with_journal()`` for the journal semantics.

        If the run halts (a callback suspends, or it times out or runs out of
        fuel), the session is rolled back to its Python globals from before the
        call, so resuming in the same session does not apply the replayed
        prefix's effects twice. The rollback works like ``snapshot_state()``
        (unserializable objects such as open files are dropped) and does not
        undo VFS writes or network calls. It costs one snapshot per call, which
        is skipped when the session has no callbacks.

        If the snapshot fails (e.g. it is too large), the call runs unprotected
        and a halt resets the session, losing its state. If the rollback itself
        fails, ``error`` is an ``InitializationError``, ``suspended`` is
        ``None``, and the session is unusable; resume with ``journal`` in a new
        session.

        Args:
            code: Python source code to execute.
            journal: A journal from a previous ``ReplayOutcome.journal``. Matching
                callbacks return their recorded results instead of running live.
                Trusted input: replayed results reach the sandbox verbatim.

        Returns:
            ReplayOutcome with the result or error, the recorded journal, the
            number of replayed callbacks, and any suspension details.

        Raises:
            ValueError: If ``journal`` is not a valid journal.
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

    def snapshot_state(self) -> StateSnapshot:
        """Capture a snapshot of the current Python state.

        The snapshot contains all user-defined variables, serialized using pickle.
        It can be saved to disk and restored later. Variables that can't be
        serialized are left out, and named in ``skipped_globals``.

        Returns:
            The serialized state (``data``) and the names of the globals left
            out (``skipped_globals``).

        Raises:
            ExecutionError: If the state cannot be serialized.

        Example:
            session.execute('x = 42')
            snapshot = session.snapshot_state()
            # Save snapshot to file
            with open('state.bin', 'wb') as f:
                f.write(snapshot.data)
        """
        ...

    def restore_state(self, snapshot: StateSnapshot | bytes) -> None:
        """Restore Python state from a previously captured snapshot.

        Args:
            snapshot: A ``StateSnapshot``, or its ``data`` bytes.

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


@final
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

    def __new__(cls, *, max_bytes: Optional[int] = None) -> VfsStorage:
        """Create a new empty VFS storage.

        Args:
            max_bytes: Cap on the total size of all file contents combined,
                plus a charge of the path length and 64 bytes for each file and
                directory. Writes that would exceed it fail inside the sandbox with
                ``OSError``. ``None`` (the default) means 64 MiB. The storage owns
                this cap, so ``ResourceLimits.max_vfs_bytes`` does not apply.
        """
        ...


@final
class PoolStats:
    """Statistics about pool usage.

    Returned by ``SandboxPool.stats()``.
    """

    @property
    def total(self) -> int:
        """Total number of sandboxes tracked by the pool (in use + idle)."""
        ...

    @property
    def idle(self) -> int:
        """Number of warm sandboxes sitting idle in the pool queue."""
        ...

    @property
    def available(self) -> int:
        """Remaining semaphore permits (concurrency capacity)."""
        ...

    @property
    def in_use(self) -> int:
        """Number of sandboxes currently in use."""
        ...

    @property
    def total_acquisitions(self) -> int:
        """Total successful acquisitions since pool creation."""
        ...

    @property
    def total_creations(self) -> int:
        """Total sandbox creations (initial + on-demand)."""
        ...

    @property
    def wait_count(self) -> int:
        """Number of acquisitions that had to wait for a sandbox."""
        ...

    @property
    def total_wait_time_ms(self) -> float:
        """Cumulative wait time in milliseconds."""
        ...

    @property
    def average_wait_time_ms(self) -> float:
        """Average wait time in milliseconds (0.0 if no waits)."""
        ...


@final
class PooledSandbox:
    """A sandbox lease acquired from a pool.

    Returns to the pool automatically when the context manager exits or
    ``release()`` is called. Use-after-release raises ``ValueError``.

    Example:
        with pool.acquire() as sandbox:
            result = sandbox.execute('print("Hello!")')
        # sandbox is returned to the pool here
    """

    def execute(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> ExecuteResult:
        """Execute Python code in the sandboxed lease. See ``Sandbox.execute()``.

        Args:
            code: Python source code to execute.
            cancel: Optional token; cancelling it stops the run.

        Returns:
            ExecuteResult containing stdout, timing info, and statistics.

        Raises:
            ValueError: If the sandbox has already been released.
            ExecutionError: If the Python code raises an exception.
            TimeoutError: If execution exceeds the timeout limit.
            ResourceLimitError: If a resource limit is exceeded.
            CancelledError: If ``cancel`` was cancelled.
        """
        ...

    def execute_async(
        self, code: str, *, cancel: Optional[CancelToken] = None
    ) -> asyncio.Future[ExecuteResult]:
        """Execute Python code in the sandboxed lease from asyncio. See
        ``Sandbox.execute_async()``.

        Raises:
            ValueError: If the sandbox has already been released.
        """
        ...

    def execute_with_journal(
        self, code: str, journal: Optional[dict[str, Any]] = None
    ) -> ReplayOutcome:
        """Execute code with callback journaling and replay. See
        ``Sandbox.execute_with_journal()``.

        Args:
            code: Python source code to execute.
            journal: A journal from a previous ``ReplayOutcome.journal``. Matching
                callbacks return their recorded results instead of running live.
                Trusted input: replayed results reach the sandbox verbatim.

        Returns:
            ReplayOutcome with the result or error, the recorded journal, the
            number of replayed callbacks, and any suspension details.

        Raises:
            ValueError: If the sandbox has already been released or ``journal``
                is not a valid journal.
        """
        ...

    def release(self) -> None:
        """Release the sandbox back to the pool.

        Idempotent -- calling release() on an already-released sandbox is a no-op.
        """
        ...

    def __enter__(self) -> PooledSandbox: ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Any,
    ) -> None: ...


@final
class SandboxPool:
    """A managed pool of warm sandbox instances.

    Provides bounded concurrent execution with automatic lease lifecycle.
    Create via ``SandboxFactory.create_pool()``.

    Example:
        factory = SandboxFactory(imports=["json"], cache=True)
        pool = factory.create_pool(max_size=4, min_idle=1)

        with pool.acquire(resource_limits=ResourceLimits(execution_timeout_ms=1000)) as sandbox:
            result = sandbox.execute('import json; print(json.dumps([1, 2]))')

        pool.close()
    """

    def acquire(
        self,
        *,
        resource_limits: Optional[ResourceLimits] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
    ) -> PooledSandbox:
        """Acquire a sandbox from the pool.

        Blocks until a sandbox is available or the acquire timeout is reached.
        The GIL is released while waiting so other Python threads can make
        progress (including releasing their own leases).

        Args:
            resource_limits: Optional per-request resource limits.
            callbacks: Optional per-request callbacks.
            on_stdout: Optional per-request stdout streaming callback.
            on_stderr: Optional per-request stderr streaming callback.

        Returns:
            A ``PooledSandbox`` that should be used as a context manager.

        Raises:
            PoolClosedError: If the pool has been closed.
            PoolTimeoutError: If the acquire timeout is reached.
        """
        ...

    def try_acquire(
        self,
        *,
        resource_limits: Optional[ResourceLimits] = None,
        callbacks: Optional[Union[CallbackRegistry, Sequence[CallbackDict]]] = None,
        on_stdout: Optional[Callable[[bytes], None]] = None,
        on_stderr: Optional[Callable[[bytes], None]] = None,
    ) -> Optional[PooledSandbox]:
        """Try to acquire a sandbox without blocking.

        Returns ``None`` if no sandbox is immediately available.

        Raises:
            PoolClosedError: If the pool has been closed.
        """
        ...

    def stats(self) -> PoolStats:
        """Get current pool statistics."""
        ...

    def evict_idle(self) -> int:
        """Evict idle sandboxes that have exceeded the idle timeout.

        Maintains at least ``min_idle`` instances. Returns the number evicted.
        """
        ...

    def close(self) -> None:
        """Close the pool, preventing new acquisitions.

        Blocked ``acquire()`` calls are woken with ``PoolClosedError``.
        Idle sandboxes are dropped. Existing leases continue to work
        but are not returned to the pool on release.
        """
        ...

    @property
    def is_closed(self) -> bool:
        """Whether the pool has been closed."""
        ...

    def __enter__(self) -> SandboxPool: ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Any,
    ) -> None: ...


__version__: str
"""Version of the pyeryx package."""
