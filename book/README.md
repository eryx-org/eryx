# Eryx Documentation

This directory contains the mdbook-based documentation for Eryx.

## Building the book

```bash
mise run book:build
```

## Serving locally

```bash
mise run book:serve
```

Then open http://localhost:3000 in your browser.

## Testing code samples

### Test Rust code blocks

```bash
mise run book:test
```

### Test Python code blocks

```bash
mise run book:test-python
```

### Test both

```bash
mise run book:test-all
```

## Writing documentation

- Markdown files are in `src/`
- The structure is defined in `src/SUMMARY.md`
- Use `<!-- langtabs-start -->` and `<!-- langtabs-end -->` to wrap multi-language code blocks
- Code blocks between these markers will be transformed into interactive tabs by mdbook-langtabs
- Language preference is saved to localStorage and persists across pages

### Multi-language code example

\`\`\`markdown
<!-- langtabs-start -->
\`\`\`rust
use eryx::Sandbox;
// Rust code here
\`\`\`

\`\`\`python
import eryx
# Python code here
\`\`\`
<!-- langtabs-end -->
\`\`\`

### Code block attributes

You can add attributes to code blocks to control testing:

- `no_run` - Don't run this code (Rust only, for mdbook test)
- `skip` - Skip testing this block (both languages)

Example:
\`\`\`markdown
\`\`\`rust,no_run
// This won't be executed during testing
\`\`\`

\`\`\`python,skip
# This won't be tested
\`\`\`
\`\`\`

## CI

The book is tested on every PR and deployed to GitHub Pages on pushes to main.
