"""Tests for the eryx Python bindings."""

from pathlib import Path

import eryx
import pytest


class TestSandbox:
    """Tests for the Sandbox class."""

    def test_create_sandbox(self):
        """Test that a sandbox can be created."""
        sandbox = eryx.Sandbox()
        assert sandbox is not None

    def test_simple_execution(self):
        """Test simple code execution."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("hello")')
        assert result.stdout == "hello"

    def test_execute_returns_result(self):
        """Test that execute returns an ExecuteResult."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("test")')
        assert isinstance(result, eryx.ExecuteResult)
        assert hasattr(result, "stdout")
        assert hasattr(result, "duration_ms")
        assert hasattr(result, "callback_invocations")
        assert hasattr(result, "peak_memory_bytes")

    def test_duration_is_positive(self):
        """Test that execution duration is tracked."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = 1 + 1")
        assert result.duration_ms > 0

    def test_multiple_prints(self):
        """Test multiple print statements."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
print("line 1")
print("line 2")
print("line 3")
""")
        assert result.stdout == "line 1\nline 2\nline 3"

    def test_arithmetic(self):
        """Test arithmetic operations."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
x = 2 + 3
y = x * 4
print(f"{x}, {y}")
""")
        assert result.stdout == "5, 20"

    def test_data_structures(self):
        """Test Python data structures work in sandbox."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
lst = [1, 2, 3]
dct = {"a": 1, "b": 2}
print(f"list: {lst}")
print(f"dict: {dct}")
""")
        assert "list: [1, 2, 3]" in result.stdout
        assert "dict: {'a': 1, 'b': 2}" in result.stdout

    def test_sandbox_isolation(self):
        """Test that sandbox is isolated from host filesystem."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import os
try:
    # Try to access host filesystem
    os.listdir("/etc")
    print("accessed")
except Exception as e:
    print(f"blocked: {type(e).__name__}")
""")
        # Should either fail or show an empty/virtual filesystem
        assert "blocked" in result.stdout or "accessed" not in result.stdout

    def test_sandbox_reuse(self):
        """Test that a sandbox can be reused for multiple executions."""
        sandbox = eryx.Sandbox()

        result1 = sandbox.execute('print("first")')
        assert result1.stdout == "first"

        result2 = sandbox.execute('print("second")')
        assert result2.stdout == "second"


class TestNetConfig:
    """Tests for NetConfig class."""

    def test_default_config(self):
        """Test default NetConfig values."""
        config = eryx.NetConfig()
        assert config.max_connections == 10
        assert config.connect_timeout_ms == 30000
        assert config.io_timeout_ms == 60000
        assert config.allowed_hosts == []
        # Blocked hosts should contain localhost and private networks
        assert "localhost" in config.blocked_hosts
        assert "127.*" in config.blocked_hosts

    def test_custom_config(self):
        """Test custom NetConfig values."""
        config = eryx.NetConfig(
            max_connections=5,
            connect_timeout_ms=5000,
            io_timeout_ms=10000,
            allowed_hosts=["api.example.com"],
            blocked_hosts=["*.local"],
        )
        assert config.max_connections == 5
        assert config.connect_timeout_ms == 5000
        assert config.io_timeout_ms == 10000
        assert config.allowed_hosts == ["api.example.com"]
        assert config.blocked_hosts == ["*.local"]

    def test_permissive_config(self):
        """Test permissive NetConfig."""
        config = eryx.NetConfig.permissive()
        assert config.max_connections == 100
        assert config.blocked_hosts == []

    def test_allow_host_chaining(self):
        """Test allow_host method chaining."""
        config = eryx.NetConfig()
        result = config.allow_host("api.example.com").allow_host("*.openai.com")
        assert "api.example.com" in result.allowed_hosts
        assert "*.openai.com" in result.allowed_hosts

    def test_block_host_chaining(self):
        """Test block_host method chaining."""
        config = eryx.NetConfig(blocked_hosts=[])
        result = config.block_host("*.local").block_host("internal.*")
        assert "*.local" in result.blocked_hosts
        assert "internal.*" in result.blocked_hosts

    def test_allow_localhost(self):
        """Test allow_localhost removes localhost from blocked."""
        config = eryx.NetConfig()
        assert "localhost" in config.blocked_hosts

        result = config.allow_localhost()
        assert "localhost" not in result.blocked_hosts
        assert "127.*" not in result.blocked_hosts

    def test_with_root_cert(self):
        """Test with_root_cert method."""
        config = eryx.NetConfig()
        # Just test it doesn't error - actual cert validation is in integration tests
        cert_bytes = b"\x00\x01\x02\x03"
        result = config.with_root_cert(cert_bytes)
        assert result is not None

    def test_sandbox_with_network(self):
        """Test creating sandbox with network config.

        Note: This may fail if the embedded runtime was built without network support.
        In that case, full networking tests are in crates/eryx/tests/tls_networking.rs.
        """
        config = eryx.NetConfig(allowed_hosts=["api.example.com"])
        try:
            sandbox = eryx.Sandbox(network=config)
            # Just verify sandbox is created - actual networking is tested in integration tests
            result = sandbox.execute('print("ok")')
            assert result.stdout == "ok"
        except eryx.InitializationError as e:
            if "eryx:sandbox/tls" in str(e) or "eryx:net" in str(e):
                pytest.skip("Embedded runtime does not include network support")
            raise

    def test_sandbox_with_permissive_network(self):
        """Test creating sandbox with permissive network config.

        Note: This may fail if the embedded runtime was built without network support.
        """
        config = eryx.NetConfig.permissive()
        try:
            sandbox = eryx.Sandbox(network=config)
            result = sandbox.execute('print("permissive ok")')
            assert result.stdout == "permissive ok"
        except eryx.InitializationError as e:
            if "eryx:sandbox/tls" in str(e) or "eryx:net" in str(e):
                pytest.skip("Embedded runtime does not include network support")
            raise

    def test_repr(self):
        """Test NetConfig repr."""
        config = eryx.NetConfig(allowed_hosts=["api.example.com"])
        repr_str = repr(config)
        assert "NetConfig" in repr_str
        assert "api.example.com" in repr_str


class TestResourceLimits:
    """Tests for ResourceLimits configuration."""

    def test_default_limits(self):
        """Test default resource limits."""
        limits = eryx.ResourceLimits()
        assert limits.execution_timeout_ms == 30000
        assert limits.callback_timeout_ms == 10000
        assert limits.max_memory_bytes == 134217728  # 128 MB
        assert limits.max_callback_invocations == 1000

    def test_custom_limits(self):
        """Test custom resource limits."""
        limits = eryx.ResourceLimits(
            execution_timeout_ms=5000,
            max_memory_bytes=50_000_000,
        )
        assert limits.execution_timeout_ms == 5000
        assert limits.max_memory_bytes == 50_000_000

    def test_unlimited(self):
        """Test unlimited resource limits."""
        limits = eryx.ResourceLimits.unlimited()
        assert limits.execution_timeout_ms is None
        assert limits.callback_timeout_ms is None
        assert limits.max_memory_bytes is None
        assert limits.max_callback_invocations is None

    def test_sandbox_with_limits(self):
        """Test creating sandbox with resource limits."""
        limits = eryx.ResourceLimits(execution_timeout_ms=10000)
        sandbox = eryx.Sandbox(resource_limits=limits)
        result = sandbox.execute('print("ok")')
        assert result.stdout == "ok"

    def test_execution_timeout(self):
        """Test that execution timeout works."""
        limits = eryx.ResourceLimits(execution_timeout_ms=500)
        sandbox = eryx.Sandbox(resource_limits=limits)

        with pytest.raises(eryx.TimeoutError):
            sandbox.execute("while True: pass")


class TestExceptions:
    """Tests for exception handling."""

    def test_execution_error_on_exception(self):
        """Test that Python exceptions become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("raise ValueError('test error')")

    def test_execution_error_on_syntax_error(self):
        """Test that syntax errors become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("def broken(")

    def test_execution_error_on_import_error(self):
        """Test that import errors become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("import nonexistent_module_xyz")

    def test_eryx_error_is_base_class(self):
        """Test that all eryx exceptions inherit from EryxError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.EryxError):
            sandbox.execute("raise RuntimeError('test')")

    def test_timeout_error_is_catchable_as_builtin(self):
        """Test that TimeoutError can be caught as Python's TimeoutError."""
        limits = eryx.ResourceLimits(execution_timeout_ms=500)
        sandbox = eryx.Sandbox(resource_limits=limits)

        with pytest.raises(TimeoutError):  # Built-in TimeoutError
            sandbox.execute("while True: pass")


class TestExecuteResult:
    """Tests for ExecuteResult class."""

    def test_result_str_returns_stdout(self):
        """Test that str(result) returns stdout."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("test output")')
        assert str(result) == "test output"

    def test_result_repr(self):
        """Test that repr(result) is informative."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("x")')
        repr_str = repr(result)
        assert "ExecuteResult" in repr_str
        assert "stdout" in repr_str

    def test_callback_invocations_zero_without_callbacks(self):
        """Test that callback_invocations is 0 when no callbacks used."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = 1")
        assert result.callback_invocations == 0

    def test_peak_memory_bytes_is_present(self):
        """Test that peak memory usage is tracked."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = [i for i in range(1000)]")
        assert result.peak_memory_bytes is not None
        assert result.peak_memory_bytes > 0


class TestModuleMetadata:
    """Tests for module-level metadata."""

    def test_version_is_string(self):
        """Test that __version__ is a string."""
        assert isinstance(eryx.__version__, str)

    def test_version_format(self):
        """Test that version follows semver-ish format."""
        parts = eryx.__version__.split(".")
        assert len(parts) >= 2
        assert all(p.isdigit() for p in parts[:2])

    def test_all_exports_exist(self):
        """Test that all __all__ exports are accessible."""
        for name in eryx.__all__:
            assert hasattr(eryx, name), f"Missing export: {name}"


# Module-level fixture for SandboxFactory tests
# Factory creation is slow (~2s), so we share one instance across tests
_shared_factory = None


def get_shared_factory():
    """Get or create a shared SandboxFactory for tests."""
    global _shared_factory
    if _shared_factory is None:
        _shared_factory = eryx.SandboxFactory()
    return _shared_factory


class TestSandboxFactory:
    """Tests for SandboxFactory class.

    Note: These tests share a single SandboxFactory instance
    because factory creation takes ~2s each time.
    """

    def test_factory_basic_creation(self):
        """Test that a SandboxFactory can be created."""
        factory = get_shared_factory()
        assert factory is not None
        assert factory.size_bytes > 0

    def test_factory_create_sandbox(self):
        """Test creating a sandbox from factory."""
        factory = get_shared_factory()
        sandbox = factory.create_sandbox()
        assert sandbox is not None

        result = sandbox.execute("print('hello')")
        assert result.stdout == "hello"

    def test_factory_multiple_sandboxes(self):
        """Test creating multiple sandboxes from same factory."""
        factory = get_shared_factory()

        sandbox1 = factory.create_sandbox()
        sandbox2 = factory.create_sandbox()

        result1 = sandbox1.execute("print('sandbox1')")
        result2 = sandbox2.execute("print('sandbox2')")

        assert result1.stdout == "sandbox1"
        assert result2.stdout == "sandbox2"

    def test_factory_sandboxes_isolated(self):
        """Test that sandboxes from same factory are isolated."""
        factory = get_shared_factory()

        sandbox1 = factory.create_sandbox()
        sandbox1.execute("shared_var = 42")

        sandbox2 = factory.create_sandbox()
        result = sandbox2.execute("""
try:
    print(f"var={shared_var}")
except NameError:
    print("isolated")
""")
        assert "isolated" in result.stdout

    def test_factory_save_and_load(self, tmp_path):
        """Test saving and loading a sandbox factory."""
        factory = get_shared_factory()
        save_path = tmp_path / "factory.bin"
        factory.save(save_path)

        assert save_path.exists()
        assert save_path.stat().st_size > 0

        # Load and use
        loaded = eryx.SandboxFactory.load(save_path)
        assert loaded.size_bytes == factory.size_bytes

        sandbox = loaded.create_sandbox()
        result = sandbox.execute("import json; print(json.dumps([1,2]))")
        assert result.stdout == "[1, 2]"

    def test_factory_to_bytes(self):
        """Test getting factory as bytes."""
        factory = get_shared_factory()
        data = factory.to_bytes()
        assert isinstance(data, bytes)
        assert len(data) == factory.size_bytes

    def test_factory_repr(self):
        """Test __repr__ is informative."""
        factory = get_shared_factory()
        repr_str = repr(factory)
        assert "SandboxFactory" in repr_str
        assert "size_bytes" in repr_str

    def test_factory_with_resource_limits_timeout(self):
        """Test creating sandbox with resource limits from factory.

        Uses epoch-based interruption which works correctly with
        pre-compiled WASM components.
        """
        factory = get_shared_factory()
        limits = eryx.ResourceLimits(execution_timeout_ms=500)
        sandbox = factory.create_sandbox(resource_limits=limits)

        with pytest.raises(eryx.TimeoutError):
            sandbox.execute("while True: pass")

    def test_factory_with_resource_limits_memory(self):
        """Test that resource limits can be set (without testing timeout)."""
        factory = get_shared_factory()
        limits = eryx.ResourceLimits(
            execution_timeout_ms=30000,
            max_memory_bytes=100_000_000,
        )
        sandbox = factory.create_sandbox(resource_limits=limits)

        # Just verify we can create and use the sandbox
        result = sandbox.execute("print('ok')")
        assert result.stdout == "ok"

    def test_factory_stdlib_imports_work(self):
        """Test that stdlib imports work with factory."""
        factory = get_shared_factory()
        sandbox = factory.create_sandbox()

        # Various stdlib modules should be importable
        result = sandbox.execute("""
import json
import re
data = json.dumps({"x": 1})
match = re.search(r"\\d+", data)
print(f"json: {data}, match: {match.group()}")
""")
        assert "json:" in result.stdout
        assert "match: 1" in result.stdout

    @pytest.mark.skipif(
        not Path("/tmp/wheels/jinja2-3.1.6-py3-none-any.whl").exists(),
        reason="jinja2 wheel not available",
    )
    def test_factory_with_packages(self):
        """Test factory with packages (if available)."""
        jinja2_wheel = "/tmp/wheels/jinja2-3.1.6-py3-none-any.whl"
        markupsafe_wheel = None

        # Find markupsafe wheel
        wheels_dir = Path("/tmp/wheels")
        for f in wheels_dir.iterdir():
            if f.name.lower().startswith("markupsafe") and f.suffix == ".whl":
                markupsafe_wheel = str(f)
                break

        if not markupsafe_wheel:
            pytest.skip("markupsafe wheel not available")

        # This creates a separate factory with packages
        factory = eryx.SandboxFactory(
            packages=[jinja2_wheel, markupsafe_wheel],
            imports=["jinja2"],
        )

        sandbox = factory.create_sandbox()
        result = sandbox.execute("""
from jinja2 import Template
t = Template("Hello {{ name }}")
print(t.render(name="PreInit"))
""")
        assert result.stdout == "Hello PreInit"


class TestNetworkIntegration:
    """Integration tests for networking functionality.

    These tests verify that the sync socket shim works correctly with
    fiber-based async on the host. They require network access and use
    real external services.
    """

    @pytest.fixture
    def network_sandbox(self):
        """Create a sandbox with permissive network config."""
        try:
            config = eryx.NetConfig.permissive()
            return eryx.Sandbox(network=config)
        except eryx.InitializationError as e:
            if "eryx:net" in str(e):
                pytest.skip("Embedded runtime does not include network support")
            raise

    def test_socket_module_imports(self, network_sandbox):
        """Test that socket module can be imported."""
        result = network_sandbox.execute("""
import socket
print(f"AF_INET={socket.AF_INET}")
print(f"SOCK_STREAM={socket.SOCK_STREAM}")
print("socket import ok")
""")
        assert "socket import ok" in result.stdout

    def test_ssl_module_imports(self, network_sandbox):
        """Test that ssl module can be imported."""
        result = network_sandbox.execute("""
import ssl
ctx = ssl.create_default_context()
print(f"SSLContext created: {type(ctx).__name__}")
print("ssl import ok")
""")
        assert "ssl import ok" in result.stdout

    def test_sync_socket_tcp_connect(self, network_sandbox):
        """Test synchronous socket.connect() works via fiber-based async."""
        result = network_sandbox.execute("""
import socket

# Create a socket and connect to a well-known service
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(10)

try:
    # Connect to example.com on port 80
    sock.connect(("example.com", 80))
    print("TCP connected")

    # Send a simple HTTP request
    request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
    sock.send(request)
    print("Request sent")

    # Read the response
    response = sock.recv(1024)
    if b"HTTP/1.1" in response or b"HTTP/1.0" in response:
        print("HTTP response received")
    else:
        print(f"Unexpected response: {response[:100]}")

    sock.close()
    print("SUCCESS")
except Exception as e:
    print(f"Error: {e}")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_sync_socket_https_with_ssl(self, network_sandbox):
        """Test synchronous HTTPS via socket + ssl.wrap_socket()."""
        result = network_sandbox.execute("""
import socket
import ssl

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(15)

try:
    # Connect to example.com on port 443
    sock.connect(("example.com", 443))
    print("TCP connected")

    # Wrap with SSL
    ctx = ssl.create_default_context()
    ssl_sock = ctx.wrap_socket(sock, server_hostname="example.com")
    print("TLS handshake complete")

    # Send HTTPS request
    request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
    ssl_sock.send(request)
    print("Request sent")

    # Read response
    response = ssl_sock.recv(1024)
    if b"HTTP/1.1" in response or b"HTTP/1.0" in response:
        print("HTTPS response received")
    else:
        print(f"Unexpected response: {response[:100]}")

    ssl_sock.close()
    print("SUCCESS")
except Exception as e:
    print(f"Error: {type(e).__name__}: {e}")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_async_tcp_api(self, network_sandbox):
        """Test the async _eryx_async API for TCP."""
        result = network_sandbox.execute("""
import _eryx_async

# Use top-level await with the async API
tcp_handle = await _eryx_async.await_tcp_connect("example.com", 80)
print(f"Connected with handle: {tcp_handle}")

request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
written = await _eryx_async.await_tcp_write(tcp_handle, request)
print(f"Wrote {written} bytes")

response = await _eryx_async.await_tcp_read(tcp_handle, 1024)
if b"HTTP" in response:
    print("Got HTTP response")

_eryx_async.tcp_close(tcp_handle)
print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_async_tls_api(self, network_sandbox):
        """Test the async _eryx_async API for TLS."""
        result = network_sandbox.execute("""
import _eryx_async

# Connect and upgrade to TLS
tcp_handle = await _eryx_async.await_tcp_connect("example.com", 443)
print("TCP connected")

tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "example.com")
print("TLS upgraded")

request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
written = await _eryx_async.await_tls_write(tls_handle, request)
print(f"Wrote {written} bytes")

response = await _eryx_async.await_tls_read(tls_handle, 1024)
if b"HTTP" in response:
    print("Got HTTPS response")

_eryx_async.tls_close(tls_handle)
print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_network_blocked_host(self):
        """Test that blocked hosts are rejected."""
        # Default config blocks localhost
        try:
            config = eryx.NetConfig()
            sandbox = eryx.Sandbox(network=config)
        except eryx.InitializationError as e:
            if "eryx:net" in str(e):
                pytest.skip("Embedded runtime does not include network support")
            raise

        result = sandbox.execute("""
import _eryx_async

try:
    tcp_handle = await _eryx_async.await_tcp_connect("localhost", 80)
    print("UNEXPECTED: Connection succeeded")
except OSError as e:
    error_str = str(e).lower()
    if "not permitted" in error_str or "blocked" in error_str:
        print("EXPECTED: Connection blocked")
    else:
        print(f"Error: {e}")
""")
        assert "EXPECTED: Connection blocked" in result.stdout, (
            f"Test failed: {result.stdout}"
        )

    def test_allowed_hosts_whitelist(self):
        """Test that allowed_hosts whitelist works."""
        try:
            config = eryx.NetConfig(allowed_hosts=["example.com"])
            sandbox = eryx.Sandbox(network=config)
        except eryx.InitializationError as e:
            if "eryx:net" in str(e):
                pytest.skip("Embedded runtime does not include network support")
            raise

        # Should be able to connect to example.com
        result = sandbox.execute("""
import _eryx_async

tcp_handle = await _eryx_async.await_tcp_connect("example.com", 80)
print(f"Connected to example.com")
_eryx_async.tcp_close(tcp_handle)
print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

        # Should NOT be able to connect to other hosts
        result = sandbox.execute("""
import _eryx_async

try:
    tcp_handle = await _eryx_async.await_tcp_connect("google.com", 80)
    print("UNEXPECTED: Connection to google.com succeeded")
except OSError as e:
    print("EXPECTED: Connection to google.com blocked")
""")
        assert "EXPECTED: Connection to google.com blocked" in result.stdout, (
            f"Test failed: {result.stdout}"
        )
