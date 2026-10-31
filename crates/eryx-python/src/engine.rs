//! The engine sandboxes run on.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::error::eryx_error_to_py;

/// The engine sandboxes run on: compiled code, the instance allocator and the
/// pool of warm instances kept ready.
///
/// Sandboxes, sessions and factories use a process-wide shared engine unless
/// given one with ``engine=``. The shared engine is configured by the
/// ``ERYX_WARM_INSTANCES``, ``ERYX_ALLOCATOR`` and ``ERYX_POOL_*``
/// environment variables. An ``Engine`` you create ignores those variables
/// entirely: anything not passed here takes the built-in default.
///
/// Each engine loads its own copy of the runtime and keeps its own warm
/// instances, and with the pooling allocator it reserves virtual address
/// space for every slot up front (terabytes with the defaults). Run one or
/// two per process, not one per sandbox.
///
/// Args:
///     warm_instances: Instances to keep ready per runtime (default 1); 0
///         disables the warm pool.
///     allocator: ``"pooling"`` (the default, except on Windows) or
///         ``"on-demand"``.
///     pool_instances: Maximum sandbox instances alive at once with the
///         pooling allocator (default 1000).
///     pool_keep_resident_mb: MiB of each linear memory kept mapped between
///         uses (default 64).
///     pool_pagemap_scan: Find dirty pages with Linux's ``PAGEMAP_SCAN``
///         (default True).
///     pool_release_interval_secs: How often to release memory kept resident
///         for unused slots (default 30); 0 never releases it.
///
/// Raises:
///     ValueError: If ``allocator`` is not a known allocator.
///     InitializationError: If a setting is invalid or the engine cannot be
///         created.
///
/// Example:
///     engine = Engine(warm_instances=4, pool_instances=256)
///     sandbox = Sandbox(engine=engine)
#[pyclass(module = "eryx", frozen)]
#[derive(Debug)]
pub struct Engine {
    pub(crate) inner: eryx::Engine,
}

#[pymethods]
impl Engine {
    #[new]
    #[pyo3(signature = (
        *,
        warm_instances = None,
        allocator = None,
        pool_instances = None,
        pool_keep_resident_mb = None,
        pool_pagemap_scan = None,
        pool_release_interval_secs = None,
    ))]
    fn new(
        warm_instances: Option<usize>,
        allocator: Option<&str>,
        pool_instances: Option<u32>,
        pool_keep_resident_mb: Option<usize>,
        pool_pagemap_scan: Option<bool>,
        pool_release_interval_secs: Option<u64>,
    ) -> PyResult<Self> {
        let mut config = eryx::EngineConfig::default();
        if let Some(n) = warm_instances {
            config = config.with_warm_instances(n);
        }
        if let Some(name) = allocator {
            config = config.with_allocator(match name {
                "pooling" => eryx::InstanceAllocator::Pooling,
                "on-demand" => eryx::InstanceAllocator::OnDemand,
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "unknown allocator '{name}', expected 'pooling' or 'on-demand'"
                    )));
                }
            });
        }
        if let Some(n) = pool_instances {
            config = config.with_pool_instances(n);
        }
        if let Some(mib) = pool_keep_resident_mb {
            config = config.with_pool_keep_resident_mb(mib);
        }
        if let Some(enabled) = pool_pagemap_scan {
            config = config.with_pool_pagemap_scan(enabled);
        }
        if let Some(secs) = pool_release_interval_secs {
            config = config.with_pool_release_interval_secs(secs);
        }
        let inner = eryx::Engine::new(config).map_err(eryx_error_to_py)?;
        Ok(Self { inner })
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.inner)
    }
}

/// The engine to use: `engine`, or the shared one.
pub(crate) fn resolve(engine: Option<&Engine>) -> PyResult<eryx::Engine> {
    resolve_shared(engine.map(|engine| &engine.inner))
}

/// `engine`, or the shared engine.
pub(crate) fn resolve_shared(engine: Option<&eryx::Engine>) -> PyResult<eryx::Engine> {
    match engine {
        Some(engine) => Ok(engine.clone()),
        None => eryx::Engine::shared().map_err(eryx_error_to_py),
    }
}
