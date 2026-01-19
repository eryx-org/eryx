"""Pytest configuration and fixtures for eryx tests."""

import ipaddress
import os
import socket
import ssl
import tempfile
import threading
from http.server import HTTPServer, SimpleHTTPRequestHandler
from pathlib import Path

import eryx
import pytest

# =============================================================================
# Test Server Fixtures
# =============================================================================


class QuietHTTPHandler(SimpleHTTPRequestHandler):
    """HTTP handler that doesn't log requests."""

    def log_message(self, format, *args):
        pass  # Suppress logging

    def _send_response(self, status, content_type, body):
        """Helper to send response with proper Content-Length."""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """Handle GET requests with test responses."""
        if self.path == "/":
            self._send_response(200, "text/plain", b"Hello from test server!")
        elif self.path == "/json":
            self._send_response(
                200, "application/json", b'{"status": "ok", "message": "test"}'
            )
        elif self.path == "/echo":
            body = f"Path: {self.path}\nHeaders: {dict(self.headers)}\n".encode()
            self._send_response(200, "text/plain", body)
        elif self.path == "/slow":
            import time

            time.sleep(2)  # Simulate slow response
            self._send_response(200, "text/plain", b"Slow response complete")
        else:
            self._send_response(404, "text/plain", b"Not found")

    def do_POST(self):
        """Handle POST requests with echo response."""
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)
        response = f'{{"received": {len(body)}, "echo": "{body.decode("utf-8", errors="replace")}"}}'
        self._send_response(200, "application/json", response.encode())


def _find_free_port():
    """Find a free port on localhost."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _generate_self_signed_cert(cert_path: Path, key_path: Path):
    """Generate a self-signed certificate for testing."""
    try:
        import datetime

        from cryptography import x509
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
        from cryptography.x509.oid import NameOID
    except ImportError:
        pytest.skip("cryptography package required for HTTPS tests")

    # Generate private key
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)

    # Generate certificate
    subject = issuer = x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "US"),
            x509.NameAttribute(NameOID.STATE_OR_PROVINCE_NAME, "Test"),
            x509.NameAttribute(NameOID.LOCALITY_NAME, "Test"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Eryx Test"),
            x509.NameAttribute(NameOID.COMMON_NAME, "localhost"),
        ]
    )

    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.datetime.utcnow())
        .not_valid_after(datetime.datetime.utcnow() + datetime.timedelta(days=1))
        .add_extension(
            x509.SubjectAlternativeName(
                [
                    x509.DNSName("localhost"),
                    x509.IPAddress(ipaddress.IPv4Address("127.0.0.1")),
                ]
            ),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )

    # Write certificate and key
    with open(cert_path, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.PEM))

    with open(key_path, "wb") as f:
        f.write(
            key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.TraditionalOpenSSL,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )

    return cert.public_bytes(serialization.Encoding.DER)


@pytest.fixture(scope="session")
def http_server():
    """Start an HTTP server for testing.

    Returns a tuple of (host, port).
    """
    port = _find_free_port()
    server = HTTPServer(("127.0.0.1", port), QuietHTTPHandler)

    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    yield ("127.0.0.1", port)

    server.shutdown()


@pytest.fixture(scope="session")
def https_server(tmp_path_factory):
    """Start an HTTPS server for testing.

    Returns a tuple of (host, port, cert_der_bytes).
    """
    import ipaddress

    # Generate certificate
    cert_dir = tmp_path_factory.mktemp("certs")
    cert_path = cert_dir / "cert.pem"
    key_path = cert_dir / "key.pem"

    cert_der = _generate_self_signed_cert(cert_path, key_path)

    port = _find_free_port()
    server = HTTPServer(("127.0.0.1", port), QuietHTTPHandler)

    # Wrap with SSL
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert_path, key_path)
    server.socket = context.wrap_socket(server.socket, server_side=True)

    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    yield ("127.0.0.1", port, cert_der)

    server.shutdown()


# =============================================================================
# Sandbox Fixtures
# =============================================================================


@pytest.fixture
def sandbox():
    """Create a basic sandbox without networking."""
    return eryx.Sandbox()


@pytest.fixture
def network_sandbox(http_server):
    """Create a sandbox with permissive network config that allows localhost.

    This fixture will FAIL (not skip) if network support is missing,
    because network support should always be available in CI.
    """
    host, port = http_server
    config = eryx.NetConfig.permissive().allow_localhost()
    return eryx.Sandbox(network=config)


@pytest.fixture
def network_sandbox_with_cert(https_server):
    """Create a sandbox with network config and custom root cert for HTTPS testing."""
    host, port, cert_der = https_server
    config = eryx.NetConfig.permissive().allow_localhost().with_root_cert(cert_der)
    return eryx.Sandbox(network=config)


# =============================================================================
# Factory Fixtures
# =============================================================================


# Module-level shared factory (expensive to create)
_shared_factory = None


@pytest.fixture(scope="session")
def sandbox_factory():
    """Get or create a shared SandboxFactory for tests.

    Factory creation takes ~2s, so we share one instance across tests.
    """
    global _shared_factory
    if _shared_factory is None:
        _shared_factory = eryx.SandboxFactory()
    return _shared_factory


# =============================================================================
# Wheel Fixtures
# =============================================================================


@pytest.fixture(scope="session")
def wheels_dir(tmp_path_factory):
    """Create a directory for test wheels and download them if needed."""
    wheels = tmp_path_factory.mktemp("wheels")

    # In CI, wheels should be pre-downloaded. For local development,
    # we'll download them on demand.
    return wheels


@pytest.fixture(scope="session")
def jinja2_wheel(wheels_dir):
    """Get the jinja2 wheel path, downloading if necessary.

    In CI, this should be pre-downloaded. For local testing, we download it.
    """
    # Check common locations
    locations = [
        Path("/tmp/wheels/jinja2-3.1.6-py3-none-any.whl"),
        wheels_dir / "jinja2-3.1.6-py3-none-any.whl",
    ]

    for loc in locations:
        if loc.exists():
            return loc

    # Try to download
    try:
        import urllib.request

        url = "https://files.pythonhosted.org/packages/62/a1/3d680cbfd5f4b8f15abc1d571870c5fc3e594bb582bc3b64ea099db13e56/jinja2-3.1.6-py3-none-any.whl"
        dest = wheels_dir / "jinja2-3.1.6-py3-none-any.whl"
        urllib.request.urlretrieve(url, dest)
        return dest
    except Exception as e:
        pytest.skip(f"Could not download jinja2 wheel: {e}")


@pytest.fixture(scope="session")
def markupsafe_wheel(wheels_dir):
    """Get the markupsafe wheel path, downloading if necessary."""
    # Check common locations
    for f in (
        Path("/tmp/wheels").glob("MarkupSafe*.whl")
        if Path("/tmp/wheels").exists()
        else []
    ):
        return f

    for f in wheels_dir.glob("MarkupSafe*.whl"):
        return f

    # Try to download (pure Python version for compatibility)
    try:
        import urllib.request

        # Note: This is the pure Python version, platform-specific builds may be faster
        url = "https://files.pythonhosted.org/packages/53/8f/f339c98a178f3c1e545622206b40986a4c3307fe39f70ccd3d9df9a9e425/MarkupSafe-3.0.2-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"
        dest = (
            wheels_dir
            / "MarkupSafe-3.0.2-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"
        )
        urllib.request.urlretrieve(url, dest)
        return dest
    except Exception as e:
        pytest.skip(f"Could not download markupsafe wheel: {e}")


def _download_packages(wheels_dir: Path, packages: list[str]) -> list[Path]:
    """Download packages using pip download.

    This is more reliable than hardcoding URLs since it handles version resolution
    and platform-appropriate wheel selection.

    Returns a list of wheel paths.
    """
    import shutil
    import subprocess

    # Find pip executable - prefer system pip since uv venvs don't include pip
    pip_path = shutil.which("pip")
    if pip_path is None:
        raise RuntimeError("pip not found in PATH")

    # Use pip to download the packages (pip download is reliable)
    result = subprocess.run(
        [
            pip_path,
            "download",
            "--only-binary=:all:",
            "-d",
            str(wheels_dir),
        ]
        + packages,
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        raise RuntimeError(f"Failed to download packages: {result.stderr}")

    # Return all .whl files in the directory
    return list(wheels_dir.glob("*.whl"))


def _filter_pure_python_wheels(wheels: list[Path]) -> list[Path]:
    """Filter out wheels with native extensions (not compatible with sandbox).

    Pure Python wheels have '-py3-none-any.whl' in their filename.
    Platform-specific wheels contain things like 'cp314-cp314-linux'.
    """
    pure_wheels = []
    for w in wheels:
        name = w.name.lower()
        # Pure Python wheels end with -py3-none-any.whl or similar
        if "-py3-none-any.whl" in name or "-py2.py3-none-any.whl" in name:
            pure_wheels.append(w)
    return pure_wheels


@pytest.fixture(scope="session")
def httpx_wheels(wheels_dir):
    """Download httpx and all its dependencies.

    Returns a list of pure Python wheel paths needed for httpx.
    httpx requires httpcore for the transport layer.
    """
    try:
        # httpx needs httpcore, and both need several other deps
        all_wheels = _download_packages(
            wheels_dir, ["httpx", "httpcore", "h11", "certifi", "idna", "sniffio"]
        )
        return _filter_pure_python_wheels(all_wheels)
    except Exception as e:
        pytest.skip(f"Could not download httpx wheels: {e}")


@pytest.fixture(scope="session")
def requests_wheels(wheels_dir):
    """Download requests and all its dependencies.

    Note: charset_normalizer has native extensions and will be filtered out.
    requests works fine without it.

    Returns a list of pure Python wheel paths needed for requests.
    """
    try:
        all_wheels = _download_packages(wheels_dir, ["requests"])
        return _filter_pure_python_wheels(all_wheels)
    except Exception as e:
        pytest.skip(f"Could not download requests wheels: {e}")


@pytest.fixture(scope="session")
def httpx_sandbox_factory(httpx_wheels):
    """Create a SandboxFactory with httpx installed."""
    return eryx.SandboxFactory(
        packages=[str(w) for w in httpx_wheels],
        imports=["httpx"],
    )


@pytest.fixture
def httpx_sandbox(httpx_sandbox_factory, http_server):
    """Create a sandbox with httpx available and network enabled."""
    host, port = http_server
    config = eryx.NetConfig.permissive().allow_localhost()
    return httpx_sandbox_factory.create_sandbox(network=config)


@pytest.fixture(scope="session")
def requests_sandbox_factory(requests_wheels):
    """Create a SandboxFactory with requests installed."""
    return eryx.SandboxFactory(
        packages=[str(w) for w in requests_wheels],
        imports=["requests"],
    )


@pytest.fixture
def requests_sandbox(requests_sandbox_factory, http_server):
    """Create a sandbox with requests available and network enabled."""
    host, port = http_server
    config = eryx.NetConfig.permissive().allow_localhost()
    return requests_sandbox_factory.create_sandbox(network=config)


# =============================================================================
# Test Markers
# =============================================================================


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line(
        "markers", "network: mark test as requiring network support"
    )
    config.addinivalue_line("markers", "slow: mark test as slow-running")
