# Secrets Management

Eryx provides secure secrets handling for sandboxed Python code. Secrets are never directly exposed to the sandbox — instead, Python code sees opaque placeholders, and real values are only substituted when making HTTP requests to authorized hosts.

## How It Works

1. You register a secret with a name, real value, and list of allowed hosts
2. Eryx generates a random placeholder (e.g., `ERYX_SECRET_PLACEHOLDER_a1b2c3...`)
3. The placeholder is set as an environment variable inside the sandbox
4. Python code reads and uses the placeholder like a normal value
5. When making HTTP requests to an allowed host, Eryx transparently substitutes the real value
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

1. **Always specify `allowed_hosts`** — Without explicit hosts, the secret may fall back to the network config's allowed hosts, which could be broader than intended.
2. **Combine with `NetConfig`** — Secrets require networking to be useful. Configure both together.
3. **Use separate secrets per service** — Each secret should have its own allowed hosts list.
4. **Don't disable scrubbing in production** — Scrubbing prevents accidental leakage through logs and output.

## Security Model

- Placeholders are randomly generated on each sandbox creation, making them unpredictable
- Host verification uses the TCP connection target, not the HTTP `Host` header (prevents spoofing)
- Scrubbing is applied to stdout, stderr, and VFS file writes
- Even if sandboxed code tries to print or write the placeholder, it appears as `[REDACTED]`

## Next Steps

- [Networking](./networking.md) — Configure network access policies
- [Sandboxes](./sandboxes.md) — Sandbox configuration
