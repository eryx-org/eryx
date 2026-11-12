//! Resource limits from Python.
//!
//! `eryx.ResourceLimits` is `eryx_run.ResourceLimits`, the frozen dataclass
//! eryx.run's client uses too, so code moves between local and hosted sandboxes
//! unchanged. This reads its fields and fills the ones left `None` with
//! pyeryx's defaults; every caller routes through here.

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use std::time::Duration;

/// The fields of an `eryx_run.ResourceLimits`.
///
/// `None` means pyeryx's default (`eryx::ResourceLimits::default()`: 30s
/// execution, 10s callbacks, 128 MiB, 1000 callback invocations, fuel off), or
/// no limit when `unlimited_by_default` is set. `max_vfs_bytes` and `max_open_host_files`
/// keep their default caps either way, since both bound host resources. The
/// default value is `ResourceLimits()`.
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    pub execution_timeout_ms: Option<u64>,
    pub callback_timeout_ms: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    pub max_callback_invocations: Option<u32>,
    pub max_fuel: Option<u64>,
    pub max_vfs_bytes: Option<u64>,
    pub max_open_host_files: Option<usize>,
    pub unlimited_by_default: bool,
}

impl<'a, 'py> FromPyObject<'a, 'py> for ResourceLimits {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let class = obj.py().import("eryx_run")?.getattr("ResourceLimits")?;
        if !obj.is_instance(&class)? {
            return Err(PyTypeError::new_err(format!(
                "resource_limits must be an eryx.ResourceLimits, not {}",
                obj.get_type().name()?
            )));
        }
        Ok(Self {
            execution_timeout_ms: obj.getattr("execution_timeout_ms")?.extract()?,
            callback_timeout_ms: obj.getattr("callback_timeout_ms")?.extract()?,
            max_memory_bytes: obj.getattr("max_memory_bytes")?.extract()?,
            max_callback_invocations: obj.getattr("max_callback_invocations")?.extract()?,
            max_fuel: obj.getattr("max_fuel")?.extract()?,
            max_vfs_bytes: obj.getattr("max_vfs_bytes")?.extract()?,
            max_open_host_files: obj.getattr("max_open_host_files")?.extract()?,
            unlimited_by_default: obj.getattr("unlimited_by_default")?.extract()?,
        })
    }
}

impl From<&ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: &ResourceLimits) -> Self {
        let mut out = if limits.unlimited_by_default {
            eryx::ResourceLimits::unlimited()
        } else {
            eryx::ResourceLimits::default()
        };
        let ms = |v: Option<u64>| v.map(Duration::from_millis);
        out.execution_timeout = ms(limits.execution_timeout_ms).or(out.execution_timeout);
        out.callback_timeout = ms(limits.callback_timeout_ms).or(out.callback_timeout);
        out.max_memory_bytes = limits.max_memory_bytes.or(out.max_memory_bytes);
        out.max_callback_invocations = limits
            .max_callback_invocations
            .or(out.max_callback_invocations);
        out.max_fuel = limits.max_fuel.or(out.max_fuel);
        out.max_vfs_bytes = limits.max_vfs_bytes.or(out.max_vfs_bytes);
        out.max_open_host_files = limits.max_open_host_files.or(out.max_open_host_files);
        out
    }
}

impl From<ResourceLimits> for eryx::ResourceLimits {
    fn from(limits: ResourceLimits) -> Self {
        (&limits).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_fields_take_defaults_or_turn_off() {
        let default: eryx::ResourceLimits = ResourceLimits::default().into();
        assert_eq!(default, eryx::ResourceLimits::default());

        let unlimited = ResourceLimits {
            execution_timeout_ms: Some(5000),
            unlimited_by_default: true,
            ..Default::default()
        };
        let unlimited: eryx::ResourceLimits = unlimited.into();
        assert_eq!(
            unlimited,
            eryx::ResourceLimits::unlimited().with_execution_timeout(Duration::from_secs(5))
        );
    }
}
