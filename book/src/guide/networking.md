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
| `http_rules` | `[]` | [Method, path and query rules](#method-path-and-query-rules) for HTTP requests |

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

### Host Patterns

`allowed_hosts`, `blocked_hosts`, `internal_hosts`, secrets' `allowed_hosts`
and HTTP rules all match host names the same way. `*` matches any run of
characters (`*.example.com`, `api.*.com`), and a pattern without `*` matches
only that host. Case, one trailing dot and IPv6 brackets are ignored on both
sides, so `API.example.com.` matches `api.example.com`, and a blocked host
stays blocked with a trailing dot. IP addresses match in their canonical form,
whatever spelling the resolver accepts: `2130706433`, `0x7f.1` and `127.1` all
match `127.0.0.1`, `::ffff:127.0.0.1` matches it too, and a zone
(`fe80::1%lo`, `127.0.0.1%lo`) is ignored.

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

## Method, Path and Query Rules

Allowing a host allows every endpoint on it. To narrow that, add HTTP rules:
a host that any rule matches only accepts requests matching one of its rules.
Hosts no rule matches are unaffected.

```python
import eryx

config = eryx.NetConfig(
    allowed_hosts=["api.github.com"],
    http_rules=[
        eryx.HttpRule("api.github.com", methods=["GET", "POST"], paths=["/repos/org/repo"]),
    ],
)
```

From Rust, use `NetConfig::allow_http(HttpRule::new(host).with_methods(..).with_paths(..).with_queries(..))`;
over gRPC, `NetworkConfig.http_rules`.

Some endpoints differ only by query. Git's smart HTTP starts a fetch with
`GET /info/refs?service=git-upload-pack` and a push with
`?service=git-receive-pack`, so allowing fetches but not pushes needs a query
rule:

```python
eryx.HttpRule(
    "git.example.com",
    methods=["GET"],
    paths=["/org/repo.git/info/refs"],
    queries=["service=git-upload-pack"],
)
```

(The fetch itself then `POST`s to `/org/repo.git/git-upload-pack`, which a
second rule allows.)

How requests match:

- **Methods** are compared ignoring case. No methods means any method.
- **Paths** match whole segments: `/repos/org/repo` matches it and anything
  under `/repos/org/repo/`, but not `/repos/org/repo-evil`. A path ending in
  `/` matches anything under it. No paths means any path.
- **Queries** are regular expressions, in the Rust
  [`regex`](https://docs.rs/regex/latest/regex/#syntax) syntax, each matched
  against the *whole* raw query string: the text after `?` exactly as sent,
  not decoded, and empty when there's none (`/x` and `/x?` both have an empty
  query). `service=git-upload-pack` matches that query and nothing else;
  `(service=git-upload-pack)?` also matches no query; `page=[0-9]+` matches
  `page=2` but not `page=2&per_page=100`. No queries means any query.
- A query a server could read differently from the pattern just doesn't match:
  a repeated parameter (`service=git-upload-pack&service=git-receive-pack`),
  a `;` separator or an escape (`service=git%2Dupload-pack`) is more text for
  an anchored pattern, not a second reading of it. Keep that so by matching
  parameter values with a class such as `[a-z-]+` rather than `.*`, which
  matches `&`, `;` and `%` too.
- Paths are matched without the query, and percent-escapes are decoded first
  (`/repos/org/%72epo` is `/repos/org/repo`).
- A path a server could resolve somewhere else never matches: one with a `.`
  or `..` segment (escaped as `%2e` or not, or with `;` parameters, as in
  `..;`), an empty segment (`//`), an escaped `/` or `\`, or a malformed
  escape. So does a target that isn't a path or an absolute URL (`CONNECT`,
  `OPTIONS *`).
- A rule's own paths must be normalized absolute paths: no such segments,
  query or escapes, and its queries must be valid regular expressions. A rule
  that breaks this is rejected when the sandbox is built.

A refused request is never sent: the write fails in the sandbox (an `OSError`
in Python) and the connection's request can't continue.

A [secret](./secrets.md#limiting-where-a-secret-is-sent) can take the same
rules to limit which requests its real value is substituted in.

Once any rule is set, every connection's HTTP/1.1 requests are parsed and
checked against both the connected host and every host the request names (its
`Host` header and an absolute-form target), so another allowed host on the
same server can't be used to reach a ruled one. Host names are compared
ignoring case, a port, a trailing dot and IPv6 brackets, and a request with
more than one `Host` header, or a `Host` that disagrees with its absolute-form
target, is refused. Requests whose framing a server
could read differently are refused: bare CR or LF, folded header lines, a
`Transfer-Encoding` other than `chunked`, both that and `Content-Length`, or
more than one length. Only HTTP/1.1 may be sent to a host with rules, so
HTTP/2, WebSockets after the upgrade, and TLS done by the sandboxed code itself
are refused there.

Every request is recognised by its request line, for any method token
(`PROPFIND` and custom methods too), and a request line preceded by an empty
line is refused, since servers skip those. Bytes that aren't a request are
refused once a request has been sent on the connection. After any refusal, the
connection refuses every later write.

Setting any rule therefore stops WebSocket traffic after an `Upgrade`, `CONNECT`
tunnels and HTTP/2 to **every** host, not only ruled ones, on connections that
start with an HTTP request: what follows isn't a request the host can check.

A first line shaped like a request (a method token, then a target starting
with `/` or `*` or containing `://`, or `HTTP/` anywhere on the line) is parsed
strictly and refused unless it's a valid HTTP/1.x request line, so HTTP/0.9
requests are refused too.

**Limitation:** rules are enforced on what the host can frame. A connection to a
host *without* rules whose first line has another shape (e.g. a Redis `SET k v`)
is passed through unparsed from then on, so other protocols keep working. One
cost of the shape rule: an inline command like that whose first argument
starts with `/` is refused instead. If sandboxed
code does its own TLS (e.g. with a pure-Python TLS library) to another allowed
host served by the same server as a ruled one, the host can't see the requests
inside, so the rules don't apply to them. Avoid allowing other names on a
server whose ruled name matters.

The JavaScript package doesn't parse requests, so it refuses `httpRules`
rather than ignoring them.

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

## `fetch` in JavaScript

JavaScript sandboxes have a `fetch` that goes through the same host-side
networking as Python's sockets: the host does the TLS and applies everything on
this page (allowed hosts, address filtering, method and path rules, timeouts,
traffic limits) and [secret substitution](./secrets.md). The guest never sees a
certificate.

```javascript
const res = await fetch("https://api.example.com/items?page=2", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "widget" }),
    signal: AbortSignal.timeout(5000),
});
if (!res.ok) throw new Error(`HTTP ${res.status}`);
const items = await res.json();
```

> **`fetch` blocks the sandbox.** The host's network calls are synchronous for
> the guest, so a `fetch` holds up the whole sandbox until its response, after
> any redirects, has been read in full. Requests never overlap:
> `Promise.all([fetch(a), fetch(b)])` runs them one after the other, and no
> other code (callbacks, other promises) runs meanwhile. For the same reason,
> nothing can call `controller.abort()` while a request is on the wire: an
> abort takes effect before the request starts. Use `AbortSignal.timeout(ms)`
> to bound a request, which passes its deadline to every network call.

What's there, and what isn't:

- `fetch(input, init)`, `Request`, `Response` (`status`, `ok`, `statusText`,
  `headers`, `url`, `redirected`, `text()`, `json()`, `arrayBuffer()`,
  `bytes()`, `clone()`, `Response.json()`), `Headers`, `AbortController` and
  `AbortSignal` (`abort()`, `timeout()`, `any()`).
- HTTP/1.1 only, one connection per request (no keep-alive), and no
  compression: requests send `accept-encoding: identity` unless the script
  sets it, and bodies aren't decompressed.
- Bodies are buffered, not streamed: `response.body` is always `null`, and a
  request body is a string, `ArrayBuffer` or typed array (no `FormData`,
  `Blob` or streams). The host's traffic limit caps the total.
- Redirects follow the fetch standard: `redirect: "follow"` (up to 20),
  `"error"` or `"manual"` (which returns the 3xx response itself). A
  301/302 POST or a 303 becomes a GET without its body, and `Authorization`
  is dropped on a redirect to another origin.
- Headers the transport owns (`Host`, `Content-Length`, `Connection`,
  `Transfer-Encoding` and the like) are set by `fetch`; ones the script
  passes are ignored.
- Host names must be ASCII: write internationalized names in their punycode
  (`xn--`) form. URLs with credentials (`https://user:pass@...`) are refused.
- A network failure or a refusal by the host's policy rejects with
  `TypeError("fetch failed")`, whose `cause.code` says why: `not-permitted`
  (a blocked host or address, or a method or path the rules don't allow),
  `timed-out`, `host-not-found`, `connection-refused`, `certificate-error`,
  `too-many-redirects`, and so on. An aborted request rejects with the
  signal's reason, a `DOMException` named `AbortError` or `TimeoutError`.
- QuickJS has no timers, so an `AbortSignal.timeout()` signal turns aborted
  when it's next looked at rather than firing its listeners on time.

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

## Traffic Limits

In Rust, `NetConfig::with_max_bytes` caps the bytes an execution sends and
receives, in all, across its connections, a shell's child processes'
included. Reads stop at the cap, and past it reads and writes raise `OSError`
(`network limit reached`); a child's request gets a `403` from its proxy, or
its connection closes if a response had started. Either way, the result's
`stats.network_bytes_sent` and `stats.network_bytes_received` say how much it
moved, children included, as the code wrote and read it. TLS overhead isn't
counted, except through a child's blind `CONNECT` tunnel (a command allowed
tunnels), where the proxy only sees the encrypted bytes: there the handshake
and record overhead count too:

```rust,ignore
let sandbox = Sandbox::embedded()
    .with_network(NetConfig::default().allow_host("api.example.com").with_max_bytes(16 << 20))
    .build()?;
let result = sandbox.execute(code).await?;
println!("{} bytes in", result.stats.network_bytes_received);
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
