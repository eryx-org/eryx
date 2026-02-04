# VFS File Scrubbing

The `eryx-vfs` crate now includes support for automatically scrubbing secret placeholders from file writes.

## Usage with Secrets

```rust
use eryx_vfs::{InMemoryStorage, ScrubbingStorage, VfsSecretConfig, VfsFileScrubPolicy};
use std::{sync::Arc, collections::HashMap};

// Create base storage
let storage = Arc::new(InMemoryStorage::new());

// Configure secrets that should be scrubbed
let mut secrets = HashMap::new();
secrets.insert("API_KEY".to_string(), VfsSecretConfig {
    placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
});

// Wrap storage with scrubbing
let scrubbing_storage = Arc::new(ScrubbingStorage::new(
    storage,
    secrets,
    VfsFileScrubPolicy::All,  // Scrub all files
));

// Use with VfsCtx as normal
let vfs_ctx = VfsCtx::new(scrubbing_storage);
```

## Scrubbing Policies

### `VfsFileScrubPolicy::All` (Default)
Scrubs all files:
```rust
VfsFileScrubPolicy::All
```

### `VfsFileScrubPolicy::None`
Disables scrubbing (useful for debugging):
```rust
VfsFileScrubPolicy::None
```

### `VfsFileScrubPolicy::Except` (Future)
Scrubs all files except specified paths:
```rust
VfsFileScrubPolicy::Except(vec![
    "/tmp/cache/*".to_string(),
    "/debug/*.log".to_string(),
])
```

### `VfsFileScrubPolicy::Only` (Future)
Only scrubs specified paths:
```rust
VfsFileScrubPolicy::Only(vec![
    "/secrets/*.txt".to_string(),
])
```

## How It Works

1. **Text files**: Placeholders are replaced via string search
2. **Binary files**: Placeholders are replaced via byte sequence search
3. **Writes are intercepted**: `write()` and `write_at()` scrub before passing to storage
4. **Reads are passthrough**: No scrubbing on reads (data is already scrubbed)

## Integration with eryx

When using `eryx` with VFS and secrets, the scrubbing happens automatically:

```rust
use eryx::{Sandbox, NetConfig};

let sandbox = Sandbox::embedded()
    .with_secret("API_KEY", "real-value", vec!["api.example.com"])
    .scrub_files(true)  // Enables VFS scrubbing
    .with_network(NetConfig::default().allow_host("api.example.com"))
    .build()?;
```

The sandbox will:
1. Pass secrets configuration to VFS
2. Wrap storage with `ScrubbingStorage`
3. All file writes automatically scrub placeholders

## Testing

Run VFS scrubbing tests:
```bash
cargo test -p eryx-vfs scrubbing
```

The tests verify:
- Text file scrubbing
- Binary file scrubbing
- Multiple placeholders
- Scrubbing can be disabled
- Byte replacement correctness
