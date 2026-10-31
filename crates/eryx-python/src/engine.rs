//! Configuration for the process-wide shared engine.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::error::eryx_error_to_py;

/// Configure the process-wide shared engine and its warm-instance pool.
///
/// The engine and the warm-instance pool are shared by every sandbox in the
/// process, so this must be called once, before the first sandbox, factory,
/// session or pool is created. Each argument left as ``None`` falls back to
/// its environment variable (``ERYX_WARM_INSTANCES``, ``ERYX_ALLOCATOR``,
/// ``ERYX_POOL_INSTANCES``, ``ERYX_POOL_KEEP_RESIDENT_MB``,
/// ``ERYX_POOL_PAGEMAP_SCAN``, ``ERYX_POOL_RELEASE_INTERVAL_SECS``), then to
/// the default.
///
/// Like rayon's ``build_global``, this configures a lazily created global;
/// it cannot change once that global exists.
///
/// Args:
///     warm_instances: Instances to keep ready per runtime; 0 disables the pool.
///     allocator: ``"pooling"`` or ``"on-demand"``.
///     pool_instances: Maximum sandbox instances alive at once (pooling only).
///     pool_keep_resident_mb: MiB of each linear memory kept mapped between uses.
///     pool_pagemap_scan: Find dirty pages with Linux's ``PAGEMAP_SCAN``.
///     pool_release_interval_secs: How often to release memory kept resident
///         for unused slots; 0 never releases it.
///
/// Raises:
///     ValueError: If ``allocator`` is not a known allocator.
///     InitializationError: If a setting is invalid, or the configuration was
///         already fixed by an earlier call or by creating a sandbox.
#[pyfunction]
#[pyo3(signature = (
    *,
    warm_instances = None,
    allocator = None,
    pool_instances = None,
    pool_keep_resident_mb = None,
    pool_pagemap_scan = None,
    pool_release_interval_secs = None,
))]
pub fn configure_shared_engine(
    warm_instances: Option<usize>,
    allocator: Option<&str>,
    pool_instances: Option<u32>,
    pool_keep_resident_mb: Option<usize>,
    pool_pagemap_scan: Option<bool>,
    pool_release_interval_secs: Option<u64>,
) -> PyResult<()> {
    let mut config = eryx::SharedEngineConfig::default();
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
    eryx::configure_shared_engine(config).map_err(eryx_error_to_py)
}
