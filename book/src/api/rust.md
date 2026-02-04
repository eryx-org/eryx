# Rust API Reference

For detailed API documentation, see the self-hosted documentation.

Suggested URL structure:
- [eryx](https://docs.eryx.run/api/rust/eryx/) - Main sandbox library
- [eryx-runtime](https://docs.eryx.run/api/rust/eryx-runtime/) - Python Wasm runtime packaging
- [eryx-vfs](https://docs.eryx.run/api/rust/eryx-vfs/) - Virtual filesystem support

## Key Types

- `Sandbox` - Main sandbox type for executing Python code
- `TypedCallback` - Strongly-typed callback trait
- `InProcessSession` - Stateful sessions with persistent Python state
- `ResourceLimits` - Resource limit configuration
- `NetConfig` - Network configuration
- `SandboxPool` - Managed pool of sandbox instances
