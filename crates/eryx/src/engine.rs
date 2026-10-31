//! The engine sandboxes run on: compiled code, the instance allocator and the
//! warm-instance pool.
//!
//! Every sandbox runs on an [`Engine`]. Unless one is passed to
//! [`SandboxBuilder::with_engine`](crate::SandboxBuilder::with_engine), that is
//! the process-wide [`Engine::shared`] engine, configured by the `ERYX_*`
//! environment variables. [`Engine::new`] builds a separate engine from an
//! [`EngineConfig`] instead, for code that wants its own allocator limits or
//! warm pool without touching the environment.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Error;
use crate::warm::WarmPool;
use crate::wasm::{AllocatorSettings, PythonExecutor, RetryableOnce};

/// How wasmtime allocates sandbox instances. See [`EngineConfig::with_allocator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstanceAllocator {
    /// Reuse pre-reserved slots (the default, except on Windows).
    Pooling,
    /// A fresh `mmap` per instance.
    OnDemand,
}

/// Settings for an [`Engine`] built with [`Engine::new`].
///
/// Only how instances are allocated and kept warm can be configured, never
/// how code is compiled, so the embedded runtime and pre-compiled artifacts
/// load into any engine.
///
/// The environment is not consulted: a setting left unset takes its built-in
/// default, whatever the matching `ERYX_*` variable says. The variables only
/// configure [`Engine::shared`].
///
/// ```
/// let config = eryx::EngineConfig::default()
///     .with_warm_instances(4)
///     .with_pool_instances(256);
/// # let _ = config;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EngineConfig {
    warm_instances: Option<usize>,
    allocator: Option<InstanceAllocator>,
    pool_instances: Option<u32>,
    pool_keep_resident_mb: Option<usize>,
    pool_pagemap_scan: Option<bool>,
    pool_release_interval_secs: Option<u64>,
}

impl EngineConfig {
    /// Instances to keep ready per runtime (default 1); `0` disables the warm
    /// pool. The shared engine's equivalent is `ERYX_WARM_INSTANCES`.
    #[must_use]
    pub fn with_warm_instances(mut self, n: usize) -> Self {
        self.warm_instances = Some(n);
        self
    }

    /// The instance allocator (default pooling, except on Windows).
    /// The shared engine's equivalent is `ERYX_ALLOCATOR`.
    #[must_use]
    pub fn with_allocator(mut self, allocator: InstanceAllocator) -> Self {
        self.allocator = Some(allocator);
        self
    }

    /// Maximum sandbox instances alive at once with the pooling allocator
    /// (default 1000). Must be positive. The shared engine's equivalent is
    /// `ERYX_POOL_INSTANCES`.
    #[must_use]
    pub fn with_pool_instances(mut self, n: u32) -> Self {
        self.pool_instances = Some(n);
        self
    }

    /// How much of each linear memory to keep mapped between uses, in MiB
    /// (default 64). The shared engine's equivalent is
    /// `ERYX_POOL_KEEP_RESIDENT_MB`.
    #[must_use]
    pub fn with_pool_keep_resident_mb(mut self, mib: usize) -> Self {
        self.pool_keep_resident_mb = Some(mib);
        self
    }

    /// Whether to find dirty pages with Linux's `PAGEMAP_SCAN` (default on).
    /// The shared engine's equivalent is `ERYX_POOL_PAGEMAP_SCAN`.
    #[must_use]
    pub fn with_pool_pagemap_scan(mut self, enabled: bool) -> Self {
        self.pool_pagemap_scan = Some(enabled);
        self
    }

    /// How often to release memory kept resident for unused slots, in seconds
    /// (default 30); `0` never releases it. The shared engine's equivalent is
    /// `ERYX_POOL_RELEASE_INTERVAL_SECS`.
    #[must_use]
    pub fn with_pool_release_interval_secs(mut self, secs: u64) -> Self {
        self.pool_release_interval_secs = Some(secs);
        self
    }

    /// The explicit setting for environment variable `name`, rendered the
    /// way the variable would be.
    ///
    /// The settings go through the same parsing as the variables, so both
    /// are validated identically.
    fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "ERYX_ALLOCATOR" => self.allocator.map(|a| {
                match a {
                    InstanceAllocator::Pooling => "pooling",
                    InstanceAllocator::OnDemand => "on-demand",
                }
                .to_string()
            }),
            "ERYX_POOL_INSTANCES" => self.pool_instances.map(|n| n.to_string()),
            "ERYX_POOL_KEEP_RESIDENT_MB" => self.pool_keep_resident_mb.map(|n| n.to_string()),
            "ERYX_POOL_PAGEMAP_SCAN" => self.pool_pagemap_scan.map(|b| u8::from(b).to_string()),
            "ERYX_POOL_RELEASE_INTERVAL_SECS" => {
                self.pool_release_interval_secs.map(|n| n.to_string())
            }
            _ => None,
        }
    }

    fn allocator_settings(&self) -> Result<AllocatorSettings, Error> {
        AllocatorSettings::from_lookup(|name| self.lookup(name))
    }
}

/// The engine sandboxes run on: a wasmtime engine with its instance allocator,
/// plus the pool of warm instances kept ready for it.
///
/// Most programs never need one: sandboxes use [`Engine::shared`] unless
/// given another with
/// [`SandboxBuilder::with_engine`](crate::SandboxBuilder::with_engine).
/// Build one with [`Engine::new`] to choose the allocator limits and warm-pool
/// size in code. Cloning is cheap and clones share everything.
///
/// Each engine compiles its own copy of every component it loads and keeps its
/// own warm instances. With the pooling allocator, it also reserves virtual
/// address space for every slot up front: terabytes with the defaults, out of
/// the 128 TiB a 64-bit Linux process has. A process should run one or two
/// engines, not one per tenant or request.
///
/// The engine is released when the last clone, and the last sandbox built on
/// it, are dropped.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    /// Distinguishes engines in the process-global instance-pre cache.
    id: u64,
    wasmtime: wasmtime::Engine,
    warm: Arc<WarmPool>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("id", &self.inner.id)
            .field(
                "pooling",
                &self.inner.wasmtime.pooling_allocator_metrics().is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Default number of ready instances per component.
    ///
    /// One is enough to hide instantiation from a caller that executes
    /// serially, which is the `SandboxFactory` render pattern; concurrent
    /// callers should raise it towards their concurrency.
    const DEFAULT_WARM_INSTANCES: usize = 1;

    /// Build an engine from `config`, ignoring the `ERYX_*` environment
    /// variables.
    ///
    /// If the pooling allocator cannot reserve its address space, the engine
    /// falls back to on-demand allocation and logs a warning.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WasmEngine`] if a setting is invalid or wasmtime
    /// cannot create the engine.
    pub fn new(config: EngineConfig) -> Result<Self, Error> {
        let allocator = config.allocator_settings()?;
        let warm = config
            .warm_instances
            .unwrap_or(Self::DEFAULT_WARM_INSTANCES);
        Self::build(&allocator, warm)
    }

    /// The process-wide engine sandboxes use by default, created on first use.
    ///
    /// Configured by the environment, read once when it is created:
    /// `ERYX_WARM_INSTANCES`, `ERYX_ALLOCATOR`, `ERYX_POOL_INSTANCES`,
    /// `ERYX_POOL_KEEP_RESIDENT_MB`, `ERYX_POOL_PAGEMAP_SCAN` and
    /// `ERYX_POOL_RELEASE_INTERVAL_SECS` (see the performance guide).
    ///
    /// # Errors
    ///
    /// Returns [`Error::WasmEngine`] if a variable is invalid or wasmtime
    /// cannot create the engine. A later call retries.
    pub fn shared() -> Result<Self, Error> {
        static SHARED: RetryableOnce<Engine> = RetryableOnce::new();
        SHARED
            .get_or_try_init(|| {
                let allocator = AllocatorSettings::from_lookup(|name| std::env::var(name).ok())?;
                Self::build(&allocator, shared_warm_instances())
            })
            .cloned()
    }

    fn build(allocator: &AllocatorSettings, warm_instances: usize) -> Result<Self, Error> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Ok(Self {
            inner: Arc::new(Inner {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                wasmtime: PythonExecutor::create_runtime_engine(allocator)?,
                warm: WarmPool::new(warm_instances),
            }),
        })
    }

    /// The underlying wasmtime engine.
    #[must_use]
    pub fn wasmtime(&self) -> &wasmtime::Engine {
        &self.inner.wasmtime
    }

    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    pub(crate) fn warm_pool(&self) -> &Arc<WarmPool> {
        &self.inner.warm
    }

    /// Whether `self` and `other` are the same engine (or clones of it).
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// `ERYX_WARM_INSTANCES`, or the default if it is unset or invalid.
fn shared_warm_instances() -> usize {
    match std::env::var("ERYX_WARM_INSTANCES") {
        Ok(v) => v.trim().parse().unwrap_or_else(|_| {
            tracing::warn!(
                value = %v,
                default = Engine::DEFAULT_WARM_INSTANCES,
                "ignoring invalid ERYX_WARM_INSTANCES, expected a non-negative integer"
            );
            Engine::DEFAULT_WARM_INSTANCES
        }),
        Err(_) => Engine::DEFAULT_WARM_INSTANCES,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_is_parsed_like_the_environment() {
        let config = EngineConfig::default()
            .with_allocator(InstanceAllocator::OnDemand)
            .with_pool_instances(3)
            .with_pool_pagemap_scan(false)
            .with_pool_release_interval_secs(0);
        let explicit = config.allocator_settings().unwrap();
        let env = AllocatorSettings::from_lookup(|name| {
            match name {
                "ERYX_ALLOCATOR" => Some("on-demand"),
                "ERYX_POOL_INSTANCES" => Some("3"),
                "ERYX_POOL_PAGEMAP_SCAN" => Some("0"),
                "ERYX_POOL_RELEASE_INTERVAL_SECS" => Some("0"),
                _ => None,
            }
            .map(str::to_string)
        })
        .unwrap();
        assert_eq!(explicit, env);
    }

    #[test]
    fn unset_settings_take_the_defaults() {
        assert_eq!(
            EngineConfig::default().allocator_settings().unwrap(),
            AllocatorSettings::default()
        );
    }

    #[test]
    fn engine_config_is_validated_like_the_environment() {
        let err = EngineConfig::default()
            .with_pool_instances(0)
            .allocator_settings()
            .expect_err("zero pool instances must be rejected");
        assert!(matches!(err, Error::WasmEngine(_)), "{err:?}");
    }
}
