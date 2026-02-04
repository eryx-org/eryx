# Contributing

Contributions are welcome! Please see the main [GitHub repository](https://github.com/eryx-org/eryx) for contribution guidelines.

## Development Setup

This project uses [mise](https://mise.jdx.dev/) for tooling:

```bash
mise install
mise run setup  # Build Wasm + precompile (one-time)
```

## Running Tests

```bash
mise run test       # Run tests with embedded Wasm
mise run test-all   # Run tests with all features
```

## Code Quality

```bash
mise run fmt        # Format code
mise run lint       # Run clippy lints
mise run ci         # Run all CI checks
```

## License

MIT OR Apache-2.0
