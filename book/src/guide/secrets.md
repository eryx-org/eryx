# Secrets Management

Eryx provides secure secrets handling for sandboxed Python code. Secrets are never directly exposed to the sandbox — instead, Python code sees opaque placeholders, and real values are only substituted when making HTTP requests to authorized hosts.

## How It Works

1. You register a secret with a name, real value, and list of allowed hosts
2. Eryx generates a random placeholder (e.g., `ERYX_SECRET_PLACEHOLDER_a1b2c3...`)
3. The placeholder is set as an environment variable inside the sandbox
4. Python code reads and uses the placeholder like a normal value
5. When making HTTPS requests to an allowed host, Eryx transparently substitutes the real value
6. Any placeholders in stdout, stderr, or files are scrubbed to `[REDACTED]`

This design means sandboxed code **cannot** exfiltrate secrets to unauthorized hosts, even if the code is malicious.

## Python API

```python
import eryx

sandbox = eryx.Sandbox(
    secrets={
        "API_KEY": {
            "value": "sk-real-secret-key",
            "allowed_hosts": ["api.example.com"],
        },
        "TOKEN": {
            "value": "ghp-abc123",
            "allowed_hosts": ["api.github.com"],
        },
    },
    network=eryx.NetConfig(allowed_hosts=["api.example.com", "api.github.com"]),
)

result = sandbox.execute("""
import os
key = os.environ["API_KEY"]
print(f"Key is: {key}")  # Prints: Key is: [REDACTED]
""")
```

Each secret dict requires a `"value"` key and accepts an optional `"allowed_hosts"` list (defaults to `[]`).

## Rust API

```rust,ignore
use eryx::{Sandbox, NetConfig};

let sandbox = Sandbox::embedded()
    .with_secret("API_KEY", "sk-real-key", vec!["api.example.com".to_string()])
    .with_network(NetConfig::default().allow_host("api.example.com"))
    .build()?;
```

The `with_secret` method takes:
- `name` — The environment variable name visible to Python
- `value` — The real secret value (never exposed to sandbox)
- `allowed_hosts` — Hosts where the real value can be sent

A secret with an empty `allowed_hosts` uses `NetConfig.allowed_hosts` instead,
but only while `allow_all_hosts` is off. A secret left with no hosts either way
is never sent anywhere, so building a sandbox with networking enabled fails.

## Host Binding

The real value is only sent to the host the connection was opened to:

- The TLS server name must be that host (ignoring case and a trailing dot);
  `wrap_socket(sock, server_hostname=...)` with any other name fails.
- A request whose `Host` header or absolute-form target names a different host
  is refused if it carries a placeholder, as is one with a malformed request
  line or a target containing whitespace, `\` or userinfo (`user@host`).

## Responses

If a server sends the real value back (an echo endpoint, or an error message
quoting the key), the sandbox sees the placeholder instead. This also works
when the value is split across reads: bytes that may be the start of a secret
are held back for up to 100 ms waiting for the rest. Only verbatim copies are
replaced; compressed, chunk-split or otherwise encoded copies are passed through
unchanged.

## Plain HTTP

Placeholders are only substituted on TLS connections (`https://`). A request over plain TCP (`http://`) is sent with the placeholder left in place, so the real value never crosses the network unencrypted.

To substitute over plain TCP as well (e.g. for a local test server), opt in on the network config:

```rust,ignore
let sandbox = Sandbox::embedded()
    .with_secret("API_KEY", "sk-real-key", vec!["localhost".to_string()])
    .with_network(NetConfig::default().allow_localhost().allow_plaintext_secrets(true))
    .build()?;
```

In Python, pass `allow_plaintext_secrets=True` to `NetConfig` (or set the attribute); over gRPC, set `network_config.allow_plaintext_secrets` (a server started with `--disallow-plaintext-secrets` rejects such requests). The real value then crosses the network unencrypted, readable by anyone on the path, so only enable it for servers you trust on a network you trust. The JavaScript bindings have no secrets support, so there is nothing to opt in to.

## Output Scrubbing

By default, when secrets are configured, Eryx scrubs placeholders from all outputs:

- **stdout** — Placeholders are replaced with `[REDACTED]`
- **stderr** — Placeholders are replaced with `[REDACTED]`
- **VFS files** — Placeholders are scrubbed from file writes

### Configuring Scrub Policy

You can disable scrubbing for individual streams (useful for debugging):

**Python:**

```python
sandbox = eryx.Sandbox(
    secrets={"KEY": {"value": "secret", "allowed_hosts": ["example.com"]}},
    scrub_stdout=False,   # Disable stdout scrubbing
    scrub_stderr=True,    # Enable stderr scrubbing (default)
    scrub_files=True,     # Enable file scrubbing (default)
)
```

**Rust:**

```rust,ignore
let sandbox = Sandbox::embedded()
    .with_secret("KEY", "secret", vec!["example.com".to_string()])
    .scrub_stdout(false)   // Disable stdout scrubbing
    .scrub_stderr(true)    // Enable stderr scrubbing (default)
    .scrub_files(true)     // Enable file scrubbing (default)
    .build()?;
```

## Best Practices

1. **Always specify `allowed_hosts`** — Without explicit hosts, the secret falls back to the network config's allowed hosts, which could be broader than intended.
2. **Combine with `NetConfig`** — Secrets require networking to be useful. Configure both together.
3. **Use separate secrets per service** — Each secret should have its own allowed hosts list.
4. **Don't disable scrubbing in production** — Scrubbing prevents accidental leakage through logs and output.

## Security Model

- Placeholders are randomly generated on each sandbox creation, making them unpredictable
- Real values are only sent over TLS unless `allow_plaintext_secrets` is set
- Host verification uses the TCP connection target; the TLS server name and the HTTP `Host` header must match it
- Real values in responses are replaced with the placeholder before the sandbox reads them
- Scrubbing is applied to stdout, stderr, and VFS file writes
- Even if sandboxed code tries to print or write the placeholder, it appears as `[REDACTED]`

## Next Steps

- [Networking](./networking.md) — Configure network access policies
- [Sandboxes](./sandboxes.md) — Sandbox configuration
