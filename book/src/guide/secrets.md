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

## JavaScript

JavaScript sandboxes read the placeholders from `process.env`, as Python reads
`os.environ`, and use them with [`fetch`](./networking.md#fetch-in-javascript):

```javascript
const res = await fetch("https://api.github.com/user", {
    headers: { Authorization: `Bearer ${process.env.TOKEN}` },
});
console.log(process.env.TOKEN); // Prints: [REDACTED]
```

`process` only exists when the sandbox has secrets, and only has `env`. The
shell exports each placeholder as a variable, for its commands; with
[child network](./sandboxes.md#external-commands), their proxy substitutes the real values.

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

To only send a secret in one header, build its `SecretConfig` with
`only_in_header` and add it with `with_secret_config`:

```rust,ignore
use eryx::{Sandbox, NetConfig, SecretConfig};

let sandbox = Sandbox::embedded()
    .with_secret_config(
        "API_KEY",
        SecretConfig::new("sk-real-key", vec!["api.example.com".to_string()])
            .only_in_header("X-Api-Key"),
    )
    .with_network(NetConfig::default().allow_host("api.example.com"))
    .build()?;
```

The header name is compared ignoring case. A request with the placeholder in
any other header is refused, as for a host the secret isn't allowed for.

A placeholder can also be the username or password of `Authorization: Basic`
credentials (as git and `requests`' `auth=(user, password)` send them): the
host decodes the base64 `user:password`, substitutes the real value under the
same host and header rules, and re-encodes it.

Each `SecretConfig` gets a new random placeholder. Where state that may hold
one outlives a sandbox (e.g. a session restored into a new sandbox for each
execution), give the secret the same one each time with
`with_placeholder`, which takes at least 32 ASCII letters, digits and
underscores. Keep it unguessable: code that knows it can have the secret
substituted without reading it.

## Host Binding

The real value is only sent to the host the connection was opened to:

- The TLS server name must be that host (ignoring case and a trailing dot);
  `wrap_socket(sock, server_hostname=...)` with any other name fails.
- A request whose `Host` header or absolute-form target names a different host
  is refused if it carries a placeholder, as is one with a malformed request
  line or a target containing whitespace, `\` or userinfo (`user@host`).
- Only the headers of requests the host can frame are substituted. A write
  that starts with an empty line, or has a malformed request line, is refused,
  and bytes that aren't a request leave the connection unparsed, with nothing
  substituted, from then on, as does a `CONNECT` or a request asking for an
  `Upgrade` once it ends. So a placeholder never reaches the server inside
  what it reads as another request's body, or a host a tunnel leads to.

## Responses

If a host a secret is allowed for sends its real value back (an echo endpoint,
or an error message quoting the key), the sandbox sees the placeholder instead.
Other hosts never receive the value, and nor does a connection whose requests
don't get values substituted (plain TCP without `allow_plaintext_secrets`), so
those responses aren't masked: a host that has the real value some other way
(it was given it outside the sandbox, say) shows it to the sandbox as it is.
That includes a value the sandbox planted itself, through another connection
that did get it substituted: a request-inspection or audit-log endpoint on the
secret's host can store it, and a connection the sandbox opens to read it back
without its first write being a request (e.g. with a space before `GET`, which
a lenient server accepts) only gets the best-effort masking of other protocols
described below. Don't allow a secret for a host that hands back what it was
sent.

Responses are masked where the sandbox's HTTP client sees the value whole:

- **Headers** are masked as one block once they've all arrived.
- **A chunked body** is masked after de-chunking, so a value split across
  chunks is masked too. It's re-chunked on the way: the body is the server's,
  masked, but the chunk sizes are eryx's.
- **Holding back:** the last bytes of a body (one less than the host's longest
  secret value) are held back so a value split across reads is whole before
  any of it reaches the sandbox. Inside a `Content-Length` body or one chunk's
  data they wait for more of it, or for its end, however long that takes, and
  in a body that ends when the connection closes, for the close. Between
  chunks they also go out after 100 ms with nothing new, so a token stream's
  events are late by at most that, and so do bytes after a `101` or a
  `CONNECT`'s `2xx` (another protocol, where nothing is substituted). They're
  held back whatever they contain, so read sizes and timing don't reveal
  whether they match a secret. A value an allowed host splits between chunks,
  or after a `101`, more than 100 ms apart, isn't masked.
- **Anything else fails closed:** on such a connection, a reply that isn't an
  HTTP/1.x response to a request the sandbox sent (another protocol, a
  response nobody asked for) or a response whose framing can't be followed
  (malformed chunks, ambiguous lengths) ends the connection: the sandbox's
  reads fail from there on, and nothing held back reaches it. (A response
  nobody asked for on an idle connection, such as a `408` before the server
  closes it, ends the stream cleanly instead.)
- **Other protocols from the start:** a connection whose first write isn't an
  HTTP request, or whose server speaks first (Postgres after `SSLRequest`,
  SMTP, Redis), never has anything substituted, now or later, so its replies
  are held back and released the same best-effort way as after a `101`
  rather than failing.

A secret without `allowed_hosts` applies to every host the sandbox may reach,
so every TLS connection then gets these HTTP-only rules. Give each secret the
`allowed_hosts` it's for.

Only verbatim copies are replaced; compressed or otherwise encoded copies are
passed through unchanged. Masking can change a body's length: a
`Content-Length` body that contains a real value reaches the sandbox with the
placeholder in its place, so its length no longer matches the header.

The sandbox picks the encoding, too: a secret allowed in `Authorization` can be
sent base64-wrapped as `Basic` credentials (e.g. `Basic b64("x:PLACEHOLDER")`),
so an allowed host that echoes request headers returns the base64 of the real
value, which isn't masked.

To also limit which endpoints a secret can reach on its host, add
[method and path rules](./networking.md#method-and-path-rules) for that host.
Like secret substitution, they only apply to requests the host can parse: TLS
done by the sandboxed code itself isn't checked.

## Plain HTTP

Placeholders are only substituted on TLS connections (`https://`). A request over plain TCP (`http://`) is sent with the placeholder left in place, so the real value never crosses the network unencrypted.

To substitute over plain TCP as well (e.g. for a local test server), opt in on the network config:

```rust,ignore
let sandbox = Sandbox::embedded()
    .with_secret("API_KEY", "sk-real-key", vec!["localhost".to_string()])
    .with_network(NetConfig::default().allow_localhost().allow_plaintext_secrets(true))
    .build()?;
```

In Python, pass `allow_plaintext_secrets=True` to `NetConfig` (or set the attribute); over gRPC, set `network_config.allow_plaintext_secrets` (the server rejects such requests unless it was started with `--allow-plaintext-secrets`). The real value then crosses the network unencrypted, readable by anyone on the path, so only enable it for servers you trust on a network you trust. The JavaScript bindings have no secrets support, so there is nothing to opt in to.

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
