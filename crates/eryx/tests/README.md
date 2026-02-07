# Integration Tests for Secrets

This directory contains integration tests for the secrets placeholder substitution feature.

## Prerequisites

Integration tests require the embedded runtime to be built. Run:

```bash
# Option 1: Full setup (builds everything)
mise run setup

# Option 2: Just precompile the runtime
mise run precompile-eryx-runtime

# Option 3: Use the test task (builds if needed)
mise run test
```

## Running the Tests

```bash
# Run all integration tests
cargo test --test secrets_integration_test --features embedded

# Run specific test
cargo test --test secrets_integration_test test_secret_substitution_in_http_request --features embedded

# Run with mise (recommended - handles setup automatically)
mise run test
```

## What's Tested

### `test_secret_substitution_in_http_request`
- Creates a mock HTTP server on localhost
- Sandbox makes HTTP request with secret in Authorization header
- Verifies:
  - ✅ Real secret is sent to server (HTTP parsing works)
  - ✅ Placeholder is scrubbed from stdout
  - ✅ Real secret never appears in output

### `test_secret_blocked_for_unauthorized_host`
- Secret restricted to `api.example.com`
- Attempts to use with `127.0.0.1`
- Verifies substitution fails for unauthorized host

### `test_placeholder_not_in_stderr`
- Prints secret to stderr
- Verifies placeholder is scrubbed with `[REDACTED]`

### `test_multiple_secrets`
- Uses two secrets in same execution
- Verifies both are scrubbed independently

### `test_scrubbing_can_be_disabled`
- Disables stdout scrubbing with `.scrub_stdout(false)`
- Verifies placeholder appears (useful for debugging)

### `test_http2_detection`
- Attempts to send HTTP/2 connection preface
- Verifies clear error message (HTTP/2 not supported with secrets)

## Test Architecture

The integration tests use a mock HTTP server (`MockHttpServer`) that:
- Runs on localhost port 18080
- Records all received HTTP requests
- Returns a simple JSON response
- Allows verification of actual secret substitution

This approach is better than mocking because it tests the full path:
1. Python socket operations
2. TCP write in ConnectionManager
3. HTTP parsing and secret substitution
4. Actual network transmission

## Troubleshooting

### "Pre-compiled runtime not found"
Run `mise run precompile-eryx-runtime` first, or use `mise run test`.

### Tests hang
The mock server might not be cleaning up properly. Restart the test.

### Connection refused errors
Expected for tests that try unauthorized hosts. The test verifies these fail gracefully.
