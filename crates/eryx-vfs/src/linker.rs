//! Linker configuration for VFS.
//!
//! This module provides functions to add VFS to a wasmtime linker,
//! replacing the default WASI filesystem implementation.

use wasmtime::component::Linker;

use crate::bindings::{VfsFilesystem, preopens, types};
use crate::hybrid::HybridVfsState;
use crate::hybrid_bindings::{
    HybridFilesystem, preopens as hybrid_preopens, types as hybrid_types,
};
use crate::storage::VfsStorage;
use crate::wasi_impl::VfsState;

/// Marker trait for types that provide VFS access.
///
/// This trait is used to get access to the VFS context from the store state.
/// Similar to how `WasiView` provides access to WASI context.
pub trait VfsView: Send {
    /// The storage backend type.
    type Storage: VfsStorage + Clone + 'static;

    /// Get access to the VFS state.
    fn vfs(&mut self) -> VfsState<'_, Self::Storage>;
}

/// Add VFS filesystem interfaces to a linker.
///
/// This adds the `wasi:filesystem/types` and `wasi:filesystem/preopens` interfaces
/// using our VFS implementation, overriding any previously added filesystem bindings.
///
/// The override only works while our `wit/deps` packages carry the same
/// `wasi:filesystem` version that `wasmtime-wasi` ships — see the note in
/// `wit/world.wit`.
///
/// # Usage
///
/// The typical pattern is to first add standard WASI bindings, then call this function
/// to override the filesystem interfaces with VFS:
///
/// ```rust,ignore
/// use eryx_vfs::{add_vfs_to_linker, InMemoryStorage, VfsCtx, VfsState, VfsView};
/// use std::sync::Arc;
///
/// struct MyState {
///     wasi: WasiCtx,
///     table: ResourceTable,
///     vfs_ctx: VfsCtx<InMemoryStorage>,
/// }
///
/// impl WasiView for MyState { /* ... */ }
///
/// impl VfsView for MyState {
///     type Storage = InMemoryStorage;
///     fn vfs(&mut self) -> VfsState<'_, Self::Storage> {
///         VfsState {
///             ctx: &mut self.vfs_ctx,
///             table: &mut self.table,
///         }
///     }
/// }
///
/// // First add standard WASI
/// wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
///
/// // Then override filesystem with VFS
/// add_vfs_to_linker(&mut linker)?;
/// ```
///
/// The linker will use VFS for filesystem operations while keeping all other
/// WASI interfaces (clocks, random, sockets, CLI, etc.) from the standard implementation.
pub fn add_vfs_to_linker<T>(linker: &mut Linker<T>) -> anyhow::Result<()>
where
    T: VfsView + 'static,
{
    // Add filesystem types with our VFS implementation
    // This will override any previously added filesystem bindings
    types::add_to_linker::<T, VfsFilesystem<T::Storage>>(linker, |state| state.vfs())?;

    // Add preopens with our VFS implementation
    preopens::add_to_linker::<T, VfsFilesystem<T::Storage>>(linker, |state| state.vfs())?;

    Ok(())
}

/// Marker trait for types that provide hybrid VFS access.
///
/// This trait is used to get access to the hybrid VFS context from the store state.
/// Similar to `VfsView`, but for hybrid VFS that routes to either VFS storage or
/// real filesystem based on path.
pub trait HybridVfsView: Send {
    /// The storage backend type for VFS paths.
    type Storage: VfsStorage + Clone + 'static;

    /// Get access to the hybrid VFS state.
    fn hybrid_vfs(&mut self) -> HybridVfsState<'_, Self::Storage>;
}

/// Add hybrid VFS filesystem interfaces to a linker.
///
/// This adds the `wasi:filesystem/types` and `wasi:filesystem/preopens` interfaces
/// using the hybrid VFS implementation, which routes filesystem operations to either
/// VFS storage (for sandboxed paths like `/data/*`) or real filesystem (for system
/// paths like `/python-stdlib/*`).
///
/// As with [`add_vfs_to_linker`], this only overrides `wasmtime-wasi`'s
/// filesystem while our `wit/deps` packages carry the same `wasi:filesystem`
/// version that `wasmtime-wasi` ships — see the note in `wit/world.wit`.
///
/// # Usage
///
/// ```rust,ignore
/// use eryx_vfs::{
///     add_hybrid_vfs_to_linker, HybridVfsCtx, HybridVfsState, HybridVfsView,
///     InMemoryStorage, RealDir, DirPerms, FilePerms,
/// };
/// use std::sync::Arc;
///
/// struct MyState {
///     wasi: WasiCtx,
///     table: ResourceTable,
///     hybrid_vfs_ctx: HybridVfsCtx<InMemoryStorage>,
/// }
///
/// impl HybridVfsView for MyState {
///     type Storage = InMemoryStorage;
///     fn hybrid_vfs(&mut self) -> HybridVfsState<'_, Self::Storage> {
///         HybridVfsState::new(&mut self.hybrid_vfs_ctx, &mut self.table)
///     }
/// }
///
/// // Configure hybrid VFS
/// let storage = Arc::new(InMemoryStorage::new());
/// let mut hybrid_vfs_ctx = HybridVfsCtx::new(storage);
///
/// // Add VFS-managed directory (sandboxed storage)
/// hybrid_vfs_ctx.add_vfs_preopen("/data", DirPerms::all(), FilePerms::all());
///
/// // Add real filesystem directory (passthrough)
/// hybrid_vfs_ctx.add_real_preopen_path(
///     "/python-stdlib",
///     "/path/to/python/stdlib",
///     DirPerms::READ,
///     FilePerms::READ,
/// )?;
///
/// // First add standard WASI
/// wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
///
/// // Then override filesystem with hybrid VFS
/// add_hybrid_vfs_to_linker(&mut linker)?;
/// ```
pub fn add_hybrid_vfs_to_linker<T>(linker: &mut Linker<T>) -> anyhow::Result<()>
where
    T: HybridVfsView + 'static,
{
    // Add filesystem types with hybrid VFS implementation
    hybrid_types::add_to_linker::<T, HybridFilesystem<T::Storage>>(linker, |state| {
        state.hybrid_vfs()
    })?;

    // Add preopens with hybrid VFS implementation
    hybrid_preopens::add_to_linker::<T, HybridFilesystem<T::Storage>>(linker, |state| {
        state.hybrid_vfs()
    })?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use wasmtime::Engine;
    use wasmtime::component::{Linker, ResourceTable};
    use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

    use super::{HybridVfsView, VfsView, add_hybrid_vfs_to_linker, add_vfs_to_linker};
    use crate::hybrid::{HybridVfsCtx, HybridVfsState};
    use crate::storage::ArcStorage;
    use crate::wasi_impl::{VfsCtx, VfsState};

    /// Store state for the linker checks below. Only its `impl`s matter — the
    /// test never instantiates a component, it just registers host functions.
    struct TestState {
        wasi: WasiCtx,
        table: ResourceTable,
        vfs: VfsCtx<ArcStorage>,
        hybrid: HybridVfsCtx<ArcStorage>,
    }

    impl WasiView for TestState {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.wasi,
                table: &mut self.table,
            }
        }
    }

    impl VfsView for TestState {
        type Storage = ArcStorage;

        fn vfs(&mut self) -> VfsState<'_, Self::Storage> {
            VfsState {
                ctx: &mut self.vfs,
                table: &mut self.table,
            }
        }
    }

    impl HybridVfsView for TestState {
        type Storage = ArcStorage;

        fn hybrid_vfs(&mut self) -> HybridVfsState<'_, Self::Storage> {
            HybridVfsState::new(&mut self.hybrid, &mut self.table)
        }
    }

    fn wasi_linker() -> Linker<TestState> {
        let engine = Engine::default();
        let mut linker = Linker::<TestState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
        linker
    }

    /// The VFS bindings must land on exactly the same linker names that
    /// `wasmtime-wasi` registers, because callers rely on `allow_shadowing` to
    /// *replace* the host filesystem rather than sit alongside it.
    ///
    /// The names include the WIT package version, so if `crates/eryx-vfs/wit`
    /// falls behind the `wasi:filesystem` version that the current
    /// `wasmtime-wasi` ships, both implementations end up registered and
    /// wasmtime's semver-compatible lookup silently picks the higher version —
    /// i.e. the host filesystem — leaving every VFS mount unreachable.
    ///
    /// A `Linker` with the default `allow_shadowing(false)` turns that drift
    /// into a "defined twice" error, so *failing* to add is what we want here.
    #[test]
    fn linker_shadows_wasmtime_wasi_filesystem() {
        for (label, result) in [
            ("add_vfs_to_linker", add_vfs_to_linker(&mut wasi_linker())),
            (
                "add_hybrid_vfs_to_linker",
                add_hybrid_vfs_to_linker(&mut wasi_linker()),
            ),
        ] {
            let err = result.err().unwrap_or_else(|| {
                panic!(
                    "{label} did not collide with wasmtime-wasi's filesystem: the \
                     wasi:filesystem version in crates/eryx-vfs/wit must be bumped to \
                     match the one wasmtime-wasi ships"
                )
            });
            let err = format!("{err:#}");
            assert!(
                err.contains("defined twice") && err.contains("wasi:filesystem/"),
                "{label} failed for an unexpected reason: {err}"
            );
        }
    }
}
