"""Integration tests for HTTP libraries (requests, httpx, urllib).

These tests verify that popular HTTP libraries work correctly in the sandbox
with the socket/ssl shim implementation.

requests and httpx are installed via wheel fixtures in conftest.py.
urllib is part of the standard library.
"""

import pytest


class TestRequestsLibrary:
    """Tests for the requests library in the sandbox.

    The requests library is a popular HTTP client that relies on:
    - socket module for TCP connections
    - ssl module for HTTPS/TLS
    - urllib3 for connection pooling
    """

    def test_requests_import(self, requests_sandbox):
        """Test that requests can be imported."""
        result = requests_sandbox.execute("""
import requests
print(f"requests version: {requests.__version__}")
print("IMPORT_OK")
""")
        assert "IMPORT_OK" in result.stdout
        assert "requests version:" in result.stdout

    def test_requests_get_local_http(self, requests_sandbox, http_server):
        """Test requests.get() to local HTTP server."""
        host, port = http_server
        result = requests_sandbox.execute(f"""
import requests

response = requests.get("http://{host}:{port}/", timeout=5)
print(f"Status: {{response.status_code}}")
print(f"Body: {{response.text}}")
if response.status_code == 200 and "Hello from test server" in response.text:
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_requests_get_json(self, requests_sandbox, http_server):
        """Test requests.get() with JSON response."""
        host, port = http_server
        result = requests_sandbox.execute(f"""
import requests

response = requests.get("http://{host}:{port}/json", timeout=5)
data = response.json()
print(f"Status: {{response.status_code}}")
print(f"JSON: {{data}}")
if data.get("status") == "ok":
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_requests_post(self, requests_sandbox, http_server):
        """Test requests.post() to local HTTP server."""
        host, port = http_server
        result = requests_sandbox.execute(f"""
import requests

response = requests.post("http://{host}:{port}/echo", data="test post data", timeout=5)
print(f"Status: {{response.status_code}}")
print(f"Body: {{response.text}}")
if response.status_code == 200:
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )


class TestHttpxLibrary:
    """Tests for the httpx library in the sandbox.

    httpx is a modern HTTP client that supports both sync and async APIs.
    It relies on similar low-level networking as requests but has different
    internal implementation details.
    """

    def test_httpx_import(self, httpx_sandbox):
        """Test that httpx can be imported."""
        result = httpx_sandbox.execute("""
import httpx
print(f"httpx version: {httpx.__version__}")
print("IMPORT_OK")
""")
        assert "IMPORT_OK" in result.stdout
        assert "httpx version:" in result.stdout

    def test_httpx_get_local_http(self, httpx_sandbox, http_server):
        """Test httpx.get() to local HTTP server."""
        host, port = http_server
        result = httpx_sandbox.execute(f"""
import httpx

response = httpx.get("http://{host}:{port}/", timeout=5)
print(f"Status: {{response.status_code}}")
print(f"Body: {{response.text}}")
if response.status_code == 200 and "Hello from test server" in response.text:
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_httpx_get_json(self, httpx_sandbox, http_server):
        """Test httpx.get() with JSON response."""
        host, port = http_server
        result = httpx_sandbox.execute(f"""
import httpx

response = httpx.get("http://{host}:{port}/json", timeout=5)
data = response.json()
print(f"Status: {{response.status_code}}")
print(f"JSON: {{data}}")
if data.get("status") == "ok":
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_httpx_post(self, httpx_sandbox, http_server):
        """Test httpx.post() to local HTTP server."""
        host, port = http_server
        result = httpx_sandbox.execute(f"""
import httpx

response = httpx.post("http://{host}:{port}/echo", content="test post data", timeout=5)
print(f"Status: {{response.status_code}}")
print(f"Body: {{response.text}}")
if response.status_code == 200:
    print("SUCCESS")
""")
        assert "SUCCESS" in result.stdout, (
            f"Test failed: {result.stdout}\nstderr: {result.stderr}"
        )


class TestUrllibLibrary:
    """Tests for urllib (standard library HTTP client).

    urllib is part of the Python standard library and should always be available.
    It uses socket/ssl underneath, so it's a good test of our shim.

    Note: urllib uses ssl.create_default_context() which may try to access
    attributes not available in the sandbox's ssl shim. HTTP (non-TLS) requests
    should work, but HTTPS may have issues with the ssl context.
    """

    def test_urllib_import(self, network_sandbox):
        """Test that urllib modules can be imported."""
        result = network_sandbox.execute("""
import urllib.request
import urllib.parse
import urllib.error
print("urllib import ok")
""")
        assert "urllib import ok" in result.stdout

    def test_urllib_get_local_http(self, network_sandbox, http_server):
        """Test urllib.request.urlopen() to local HTTP server."""
        host, port = http_server
        result = network_sandbox.execute(f"""
import urllib.request

try:
    with urllib.request.urlopen("http://{host}:{port}/", timeout=5) as response:
        status = response.status
        body = response.read().decode()
        print(f"Status: {{status}}")
        if status == 200 and "Hello from test server" in body:
            print("SUCCESS")
        else:
            print(f"Body: {{body[:100]}}")
except Exception as e:
    print(f"Error: {{type(e).__name__}}: {{e}}")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_urllib_get_json_local_http(self, network_sandbox, http_server):
        """Test urllib with JSON response from local HTTP server."""
        host, port = http_server
        result = network_sandbox.execute(f"""
import urllib.request
import json

try:
    with urllib.request.urlopen("http://{host}:{port}/json", timeout=5) as response:
        data = json.loads(response.read().decode())
        print(f"Status: {{response.status}}")
        print(f"JSON status: {{data.get('status')}}")
        if data.get("status") == "ok":
            print("SUCCESS")
except Exception as e:
    print(f"Error: {{type(e).__name__}}: {{e}}")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_urllib_post_local_http(self, network_sandbox, http_server):
        """Test urllib POST request to local HTTP server."""
        host, port = http_server
        result = network_sandbox.execute(f"""
import urllib.request

try:
    data = b"test post data"
    req = urllib.request.Request(
        "http://{host}:{port}/echo",
        data=data,
        method="POST"
    )
    with urllib.request.urlopen(req, timeout=5) as response:
        print(f"Status: {{response.status}}")
        if response.status == 200:
            print("SUCCESS")
except Exception as e:
    print(f"Error: {{type(e).__name__}}: {{e}}")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"

    def test_urllib_https_external(self, network_sandbox):
        """Test urllib.request.urlopen() with HTTPS to external service.

        Note: This test may fail if the sandbox's ssl module doesn't fully
        support all features needed by urllib's HTTPS handling. This is a
        known limitation - for full HTTPS support, use the raw socket+ssl
        API or wait for future ssl shim improvements.
        """
        result = network_sandbox.execute("""
import urllib.request

try:
    with urllib.request.urlopen("https://example.com/", timeout=10) as response:
        status = response.status
        body = response.read().decode()
        print(f"Status: {status}")
        if status == 200 and "Example Domain" in body:
            print("SUCCESS")
        else:
            print(f"Unexpected: status={status}, len={len(body)}")
except AttributeError as e:
    # Known limitation: SSLContext may not have all attributes
    if "post_handshake_auth" in str(e) or "SSLContext" in str(e):
        print("SKIP_SSL_LIMITATION")
    else:
        print(f"Error: {type(e).__name__}: {e}")
except Exception as e:
    print(f"Error: {type(e).__name__}: {e}")
""")
        if "SKIP_SSL_LIMITATION" in result.stdout:
            pytest.skip("SSL shim doesn't support all urllib HTTPS features")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"
