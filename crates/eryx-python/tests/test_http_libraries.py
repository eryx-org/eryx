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

    def test_requests_get_https_external(self, requests_sandbox):
        """Test requests.get() with HTTPS to an external service.

        Regression test for the missing ssl.VERIFY_X509_PARTIAL_CHAIN constant
        (#231). urllib3 (which requests depends on) imports that constant via a
        single ``from ssl import (...)`` statement, so its absence aborted the
        whole import and silently disabled SSL, breaking every HTTPS request.
        None of the other network tests exercise the requests/urllib3 HTTPS
        path, so this guards against a recurrence.

        Tries several hosts so a single slow/unreachable host doesn't flake CI
        (matches the urllib external test and #224). The most reliable host is
        tried first, and the per-request timeout is honored by the sandbox (it
        is propagated to the host socket calls), so a slow host fails fast and
        falls through well within the execution budget rather than hanging until
        the sandbox-level timeout.
        """
        result = requests_sandbox.execute("""
import requests

urls = [
    "https://example.com",
    "https://www.google.com",
    "https://httpbin.org/get",
]

for url in urls:
    try:
        response = requests.get(url, timeout=5)
        if response.status_code == 200:
            print(f"SUCCESS via {url}")
            break
    except Exception as e:
        print(f"Failed {url}: {type(e).__name__}: {e}")
else:
    print("All URLs failed")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"


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

    def test_httpx_get_https_external(self, httpx_sandbox):
        """Test httpx.get() with HTTPS to an external service.

        Regression test for the missing SSLSocket._sslobj attribute. httpcore
        reads sock._sslobj via get_extra_info("ssl_object") to detect ALPN/HTTP2
        after the handshake; without it every httpx HTTPS request failed with
        AttributeError. Distinct from the requests/urllib3 path (#231).

        Tries several hosts so a single slow/unreachable host doesn't flake CI
        (matches the urllib external test and #224). The most reliable host is
        tried first, and the per-request timeout is honored by the sandbox (it
        is propagated to the host socket calls), so a slow host fails fast and
        falls through well within the execution budget rather than hanging until
        the sandbox-level timeout.
        """
        result = httpx_sandbox.execute("""
import httpx

urls = [
    "https://example.com",
    "https://www.google.com",
    "https://httpbin.org/get",
]

for url in urls:
    try:
        response = httpx.get(url, timeout=5)
        if response.status_code == 200:
            print(f"SUCCESS via {url}")
            break
    except Exception as e:
        print(f"Failed {url}: {type(e).__name__}: {e}")
else:
    print("All URLs failed")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"


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

    def test_urllib_get_no_explicit_timeout(self, network_sandbox, http_server):
        """Regression test for #246: bare urlopen (no timeout=) must not raise.

        When no explicit ``timeout=`` is passed, ``http.client`` connects with
        ``socket._GLOBAL_DEFAULT_TIMEOUT`` — an ``object()`` sentinel, not
        ``None``. The socket shim used to store the sentinel verbatim, so
        ``_timeout_ms`` hit ``sentinel <= 0`` and raised
        ``TypeError: '<=' not supported between instances of 'object' and 'int'``.
        The shim now normalizes the sentinel to "no timeout" (use the host
        default). Uses the local server so the test is hermetic.
        """
        host, port = http_server
        result = network_sandbox.execute(f"""
import urllib.request

try:
    with urllib.request.urlopen("http://{host}:{port}/") as response:
        status = response.status
        body = response.read().decode()
        if status == 200 and "Hello from test server" in body:
            print("SUCCESS")
        else:
            print(f"Unexpected: {{status}} {{body[:100]}}")
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

        The most reliable host is tried first, and the per-request timeout is
        honored by the sandbox (it is propagated to the host socket calls), so a
        slow host fails fast and falls through well within the execution budget.
        """
        result = network_sandbox.execute("""
import urllib.request

urls = [
    "https://example.com",
    "https://www.google.com",
    "https://httpbin.org/get",
]

for url in urls:
    try:
        with urllib.request.urlopen(url, timeout=5) as response:
            if response.status == 200:
                print(f"SUCCESS via {url}")
                break
    except Exception as e:
        print(f"Failed {url}: {type(e).__name__}: {e}")
else:
    print("All URLs failed")
""")
        assert "SUCCESS" in result.stdout, f"Test failed: {result.stdout}"
