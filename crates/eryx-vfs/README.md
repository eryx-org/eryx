# eryx-vfs

Virtual Filesystem for the Eryx Python sandbox.

This crate provides a custom `wasi:filesystem` implementation backed by pluggable storage backends, allowing sandboxed Python code to read and write files that persist across sandbox executions.

## Architecture

The VFS consists of several key components:

- **`VfsStorage`** - A trait for pluggable storage backends
- **`InMemoryStorage`** - An in-memory implementation for testing and ephemeral use
- **`HybridVfsCtx`** - Combines real filesystem access with virtual storage
- **`ScrubbingStorage`** - A wrapper that scrubs secret placeholders from writes

## Usage

### Basic In-Memory VFS

```rust
use eryx_vfs::{InMemoryStorage, VfsCtx, add_vfs_to_linker};
use std::sync::Arc;

// Create storage
let storage = Arc::new(InMemoryStorage::new());

// Create VFS context
let vfs_ctx = VfsCtx::new(storage);

// Add to wasmtime linker (after adding wasmtime-wasi)
wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
add_vfs_to_linker(&mut linker)?;
```

### Hybrid Filesystem

The hybrid VFS allows combining real filesystem access with virtual storage:

```rust
use eryx_vfs::{HybridVfsCtx, HybridPreopen, RealDir};

let hybrid_ctx = HybridVfsCtx::new()
    .with_preopen(HybridPreopen::Real(RealDir {
        host_path: "/path/on/host".into(),
        guest_path: "/data".into(),
        dir_perms: DirPerms::all(),
        file_perms: FilePerms::all(),
    }));
```

## Secret Scrubbing

The `ScrubbingStorage` wrapper can automatically scrub secret placeholders from file writes, preventing accidental leakage of sensitive data:

```rust
use eryx_vfs::{InMemoryStorage, ScrubbingStorage, VfsSecretConfig, VfsFileScrubPolicy};
use std::collections::HashMap;

let storage = InMemoryStorage::new();

let mut secrets = HashMap::new();
secrets.insert("API_KEY".to_string(), VfsSecretConfig {
    placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
});

let scrubbing_storage = ScrubbingStorage::new(
    storage,
    secrets,
    VfsFileScrubPolicy::All,
);
```

### Scrubbing Policies

- `VfsFileScrubPolicy::All` - Scrub all files (default)
- `VfsFileScrubPolicy::None` - Disable scrubbing
- `VfsFileScrubPolicy::Except(paths)` - Scrub all except specified paths (future)
- `VfsFileScrubPolicy::Only(paths)` - Only scrub specified paths (future)

## Testing

```bash
cargo test -p eryx-vfs
```
