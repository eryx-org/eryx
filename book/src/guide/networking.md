# Networking

By default, sandboxes have no network access. You can enable and configure networking by providing a `NetConfig` when creating a sandbox.

## Enabling Network Access

<!-- langtabs-start -->
```rust
# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, NetConfig};

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_network(NetConfig::default().allow_host("example.com"))
        .build()?;

    let result = sandbox.execute(r#"
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(10)
sock.connect(("example.com", 80))
sock.send(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
response = sock.recv(100)
sock.close()
print("Connected!" if b"HTTP" in response else "Failed")
    "#).await?;

    println!("{}", result.stdout_text());

    Ok(())
}
```

```python
import eryx

# Enable networking, allowing connections to example.com
config = eryx.NetConfig(allowed_hosts=["example.com"])
sandbox = eryx.Sandbox(network=config)

result = sandbox.execute("""
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(10)
sock.connect(("example.com", 80))
sock.send(b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n")
response = sock.recv(100)
sock.close()
print("Connected!" if b"HTTP" in response else "Failed")
""")

print(result.stdout_text)  # "Connected!"
```
<!-- langtabs-end -->

## Default Network Configuration

The default `NetConfig` has sensible security defaults:

| Setting | Default Value | Description |
|---------|---------------|-------------|
| `max_connections` | 10 | Maximum simultaneous connections |
| `connect_timeout_ms` | 30,000 | Connection timeout (30 seconds) |
| `io_timeout_ms` | 60,000 | Read/write timeout (60 seconds) |
| `allow_all_hosts` | `false` | When true, all non-blocked hosts are allowed |
| `allowed_hosts` | `[]` | Whitelist of allowed hosts (used when `allow_all_hosts` is false) |
| `blocked_hosts` | localhost, private networks | Host patterns that are always refused |
| `blocked_networks` | every non-public range | CIDRs that resolved addresses must not fall in |
| `allowed_networks` | `[]` | CIDRs exempt from `blocked_networks` |
| `internal_hosts` | `[]` | Host patterns allowed to resolve into `blocked_networks` |
| `allow_plaintext_secrets` | `false` | Also substitute [secrets](./secrets.md#plain-http) over plain TCP (sends them unencrypted) |

A bare `NetConfig()` allows no hosts: list them in `allowed_hosts`, or pass
`allow_all_hosts=True` to allow any host that isn't blocked.

```python
import eryx

config = eryx.NetConfig()
print(f"Max connections: {config.max_connections}")       # 10
print(f"Connect timeout: {config.connect_timeout_ms}ms")  # 30000
print(f"I/O timeout: {config.io_timeout_ms}ms")           # 60000
print(f"Blocked hosts: {config.blocked_hosts}")           # ['localhost', '127.*', ...]
```

## Host Filtering

### Allowing Specific Hosts (Whitelist)

When you specify `allowed_hosts`, only those hosts can be accessed:

```python
import eryx

# Only allow connections to specific hosts
config = eryx.NetConfig(allowed_hosts=["api.example.com", "api.openai.com"])
sandbox = eryx.Sandbox(network=config)

# This works
result = sandbox.execute("""
import _eryx_async
tcp = await _eryx_async.await_tcp_connect("api.example.com", 80)
print("Connected to api.example.com")
_eryx_async.tcp_close(tcp)
""")

# This fails - google.com is not in the allowed list
result = sandbox.execute("""
import _eryx_async
try:
    tcp = await _eryx_async.await_tcp_connect("google.com", 80)
except OSError as e:
    print(f"Blocked: {e}")
""")
```

### Blocking Specific Hosts

Block specific hosts while allowing others:

```python
import eryx

# Allow any host, start fresh with no blocked hosts, then add some
config = eryx.NetConfig(allow_all_hosts=True, blocked_hosts=[])
config = config.block_host("*.internal.corp").block_host("*.local")

sandbox = eryx.Sandbox(network=config)
```

### Allowing Localhost

By default, localhost is blocked for security. You can explicitly allow it:

```python
import eryx

config = eryx.NetConfig(allowed_hosts=["localhost"])
print("localhost" in config.blocked_hosts)  # True

# Allow localhost connections
config = config.allow_localhost()
print("localhost" in config.blocked_hosts)  # False

sandbox = eryx.Sandbox(network=config)
```

`allow_localhost()` also removes the IPv4 loopback range (`127.0.0.0/8`) from
`blocked_networks` and adds `::1/128` to `allowed_networks`.

## Address Filtering

Host patterns only see the name the sandbox asked for. A public name can point
at an internal address (`evil.example` → `169.254.169.254`), so every
connection is also checked by the address it resolves to. The sandbox connects
only to an address that passed the check, so a DNS answer can't swap in a
different one.

By default `blocked_networks` covers every range that isn't publicly routable:
loopback, private (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`,
`fc00::/7`), link-local (`169.254.0.0/16`, `fe80::/10`, which includes cloud
metadata endpoints), carrier-grade NAT (`100.64.0.0/10`), `0.0.0.0/8`,
IPv4-compatible (`::/96`, which includes `::` and `::1`), site-local
(`fec0::/10`), Teredo (`2001::/32`), documentation (`2001:db8::/32`),
multicast, reserved, benchmarking, local-use NAT64 and 6to4 ranges.
IPv4-mapped (`::ffff:127.0.0.1`), IPv4-compatible (`::a00:1`), NAT64
(`64:ff9b::a00:1`) and Teredo addresses are also checked by the IPv4 address
they stand for. Refusals tell sandboxed code
only that the host resolves to a blocked network; the address is logged on the
host at debug level. This applies even with
`allow_all_hosts`, and even when a host matches `allowed_hosts`.

### Allowing Internal Addresses

To reach an internal service on purpose, either exempt its addresses or trust
its name:

```python
import eryx

# Exempt a CIDR, e.g. one internal subnet
config = eryx.NetConfig(allowed_hosts=["metrics.internal.example.com"])
config = config.allow_network("10.96.0.0/12")

# Or trust a name routed internally (e.g. through cluster DNS), whatever
# address it resolves to. Only list names whose DNS you control.
config = eryx.NetConfig(allowed_hosts=["api.example.com"])
config = config.allow_internal_host("api.example.com")
print(config.internal_hosts)  # ['api.example.com']
```

`internal_hosts` only skips the address check: the host must still pass
`blocked_hosts` and `allowed_hosts`. Prefer it over `allowed_networks` for
Kubernetes services, whose cluster IPs can change.

From Rust, `NetConfig::with_resolver` replaces the system resolver with your
own [`Resolver`](https://docs.rs/eryx/latest/eryx/trait.Resolver.html) (for
example to use specific nameservers). The address check still applies to
whatever it returns.

In JavaScript, the same options are `blockedNetworks`, `allowedNetworks` and
`internalHosts`. They're enforced in Node.js only: browsers don't expose the
address a request connects to.

## Permissive Configuration

For testing or trusted environments, use permissive mode:

```python
import eryx

# No restrictions (use with caution!)
config = eryx.NetConfig.permissive()
print(f"Max connections: {config.max_connections}")  # 100
print(f"Blocked hosts: {config.blocked_hosts}")      # []
print(f"Blocked networks: {config.blocked_networks}")  # []

sandbox = eryx.Sandbox(network=config)
```

## Timeout Configuration

Configure connection and I/O timeouts:

```python
import eryx

config = eryx.NetConfig(
    allowed_hosts=["api.example.com"],
    connect_timeout_ms=5000,   # 5 second connection timeout
    io_timeout_ms=10000,       # 10 second read/write timeout
)

sandbox = eryx.Sandbox(network=config)
```

## Using HTTP Libraries

Eryx provides socket and SSL shims that allow popular Python HTTP libraries to work:

### Using `requests`

```python
import eryx

# Create a sandbox factory with requests installed
factory = eryx.SandboxFactory(
    packages=["path/to/requests.whl", "path/to/urllib3.whl", ...],
    imports=["requests"],
)

config = eryx.NetConfig.permissive()
sandbox = factory.create_sandbox(network=config)

result = sandbox.execute("""
import requests
response = requests.get("https://api.example.com/data", timeout=5)
print(f"Status: {response.status_code}")
print(f"Body: {response.json()}")
""")
```

### Using `httpx`

```python
import eryx

factory = eryx.SandboxFactory(
    packages=["path/to/httpx.whl", ...],
    imports=["httpx"],
)

config = eryx.NetConfig.permissive()
sandbox = factory.create_sandbox(network=config)

result = sandbox.execute("""
import httpx
response = httpx.get("https://example.com/", timeout=5)
print(f"Status: {response.status_code}")
""")
```

### Using `urllib` (Standard Library)

```python
import eryx

config = eryx.NetConfig.permissive()
sandbox = eryx.Sandbox(network=config)

result = sandbox.execute("""
import urllib.request

with urllib.request.urlopen("https://example.com/", timeout=10) as response:
    print(f"Status: {response.status}")
    body = response.read().decode()
    if "Example Domain" in body:
        print("Got expected content")
""")
```

## TLS/SSL Support

Eryx includes a TLS implementation that provides secure HTTPS connections:

```python
import eryx

config = eryx.NetConfig.permissive()
sandbox = eryx.Sandbox(network=config)

result = sandbox.execute("""
import socket
import ssl

# Create TCP connection
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(15)
sock.connect(("example.com", 443))

# Upgrade to TLS
ctx = ssl.create_default_context()
ssl_sock = ctx.wrap_socket(sock, server_hostname="example.com")
print("TLS handshake complete")

# Send HTTPS request
ssl_sock.send(b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n")
response = ssl_sock.recv(1024)
ssl_sock.close()

print("Got HTTPS response" if b"HTTP" in response else "Failed")
""")
```

`server_hostname` must be the host passed to `connect()`, ignoring case and a
trailing dot; any other name fails the upgrade with `PermissionError`. The
certificate is checked for the host the policy allowed, so a connection to one
allowed address can't reach a different site behind it. As a result, HTTPS
through an HTTP `CONNECT` proxy tunnel isn't supported.

### Custom Root Certificates

Add custom CA certificates for internal services:

```python
import eryx

# Load your custom CA certificate
with open("internal-ca.pem", "rb") as f:
    ca_cert = f.read()

config = eryx.NetConfig(allowed_hosts=["internal.example.com"]).with_root_cert(ca_cert)
sandbox = eryx.Sandbox(network=config)
```

## Low-Level Async API

For advanced use cases, Eryx exposes a low-level async networking API via the `_eryx_async` module:

```python
import eryx

config = eryx.NetConfig.permissive()
sandbox = eryx.Sandbox(network=config)

result = sandbox.execute("""
import _eryx_async

# TCP connection
tcp_handle = await _eryx_async.await_tcp_connect("example.com", 80)
print(f"Connected with handle: {tcp_handle}")

# Send data
request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
written = await _eryx_async.await_tcp_write(tcp_handle, request)
print(f"Wrote {written} bytes")

# Read response
response = await _eryx_async.await_tcp_read(tcp_handle, 1024)
print(f"Read {len(response)} bytes")

# Close
_eryx_async.tcp_close(tcp_handle)
print("Connection closed")
""")
```

### TLS with Low-Level API

```python
import eryx

config = eryx.NetConfig.permissive()
sandbox = eryx.Sandbox(network=config)

result = sandbox.execute("""
import _eryx_async

# Connect TCP first
tcp_handle = await _eryx_async.await_tcp_connect("example.com", 443)

# Upgrade to TLS
tls_handle = await _eryx_async.await_tls_upgrade(tcp_handle, "example.com")
print("TLS upgraded")

# Use TLS connection
request = b"GET / HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n"
await _eryx_async.await_tls_write(tls_handle, request)
response = await _eryx_async.await_tls_read(tls_handle, 1024)

_eryx_async.tls_close(tls_handle)
print(f"Got {len(response)} bytes via HTTPS")
""")
```

## Connection Limits

Limit the number of simultaneous connections:

```python
import eryx

config = eryx.NetConfig(allow_all_hosts=True, max_connections=5)
sandbox = eryx.Sandbox(network=config)

# Attempting more than 5 connections will fail
```

## Best Practices

1. **Use allowlists in production** - Only allow connections to known, trusted hosts
2. **Keep localhost blocked** - Unless you specifically need it for testing
3. **Set appropriate timeouts** - Prevent hung connections from blocking execution
4. **Limit connections** - Prevent resource exhaustion from too many connections
5. **Use HTTPS** - Always prefer encrypted connections for sensitive data

## Next Steps

- [Sandboxes](./sandboxes.md) - Creating and configuring sandboxes
- [Resource Limits](./resource-limits.md) - Additional execution constraints
- [Packages](./packages.md) - Installing HTTP libraries and other packages
