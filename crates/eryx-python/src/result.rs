//! ExecuteResult wrapper for Python.
//!
//! Exposes sandbox execution results to Python with appropriate types.

use pyo3::prelude::*;

/// Result of executing Python code in the sandbox.
///
/// This class is returned by `Sandbox.execute()` and contains the output,
/// timing information, and execution statistics.
#[pyclass(frozen, module = "eryx")]
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    /// Complete stdout output from the sandboxed code.
    #[pyo3(get)]
    pub stdout: String,

    /// Execution duration in milliseconds.
    #[pyo3(get)]
    pub duration_ms: f64,

    /// Number of callback invocations during execution.
    #[pyo3(get)]
    pub callback_invocations: u32,

    /// Peak memory usage in bytes (if available).
    #[pyo3(get)]
    pub peak_memory_bytes: Option<u64>,
}

#[pymethods]
impl ExecuteResult {
    fn __repr__(&self) -> String {
        format!(
            "ExecuteResult(stdout={:?}, duration_ms={:.2}, callback_invocations={}, peak_memory_bytes={:?})",
            truncate_string(&self.stdout, 50),
            self.duration_ms,
            self.callback_invocations,
            self.peak_memory_bytes,
        )
    }

    fn __str__(&self) -> String {
        self.stdout.clone()
    }
}

impl From<eryx::ExecuteResult> for ExecuteResult {
    fn from(result: eryx::ExecuteResult) -> Self {
        Self {
            stdout: result.stdout,
            duration_ms: result.stats.duration.as_secs_f64() * 1000.0,
            callback_invocations: result.stats.callback_invocations,
            peak_memory_bytes: result.stats.peak_memory_bytes,
        }
    }
}

/// Truncate a string for display, adding "..." if truncated.
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
