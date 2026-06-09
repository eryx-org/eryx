//! ExecuteResult wrapper for Python.
//!
//! Exposes sandbox execution results to Python with appropriate types.

use pyo3::prelude::*;

/// Result of executing Python code in the sandbox.
///
/// This class is returned by `Sandbox.execute()` and contains the output,
/// timing information, and execution statistics.
#[pyclass(frozen, module = "eryx", from_py_object)]
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    /// Complete stdout output from the sandboxed code.
    #[pyo3(get)]
    pub stdout: String,

    /// Complete stderr output from the sandboxed code.
    #[pyo3(get)]
    pub stderr: String,

    /// Execution duration in milliseconds.
    #[pyo3(get)]
    pub duration_ms: f64,

    /// Number of callback invocations during execution.
    #[pyo3(get)]
    pub callback_invocations: u32,

    /// Peak memory usage in bytes (if available).
    #[pyo3(get)]
    pub peak_memory_bytes: Option<u64>,

    /// Fuel (WASM instructions) consumed during execution (if available).
    #[pyo3(get)]
    pub fuel_consumed: Option<u64>,

    /// JSON-serialized value of the script's result variable, or `None` if it
    /// was not set. Exposed to Python as the parsed value via the `result`
    /// property; the raw JSON string is available via `result_json`.
    pub result_json: Option<String>,

    /// Reason result capture failed (e.g. the value was not JSON-serializable),
    /// or `None` when capture succeeded or no result variable was set.
    #[pyo3(get)]
    pub result_error: Option<String>,
}

#[pymethods]
impl ExecuteResult {
    /// The script's `result` variable, parsed from JSON into a native Python
    /// value, or `None` if the variable was not set. See `result_error` if the
    /// value could not be captured.
    #[getter]
    fn result(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.result_json {
            Some(json) => {
                let json_mod = py.import("json")?;
                Ok(json_mod.call_method1("loads", (json.as_str(),))?.unbind())
            }
            None => Ok(py.None()),
        }
    }

    /// The raw JSON string of the captured `result` variable, or `None`.
    #[getter]
    fn result_json(&self) -> Option<String> {
        self.result_json.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "ExecuteResult(stdout={:?}, stderr={:?}, duration_ms={:.2}, callback_invocations={}, peak_memory_bytes={:?}, fuel_consumed={:?}, result={:?}, result_error={:?})",
            truncate_string(&self.stdout, 50),
            truncate_string(&self.stderr, 50),
            self.duration_ms,
            self.callback_invocations,
            self.peak_memory_bytes,
            self.fuel_consumed,
            self.result_json.as_deref().map(|s| truncate_string(s, 50)),
            self.result_error,
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
            stderr: result.stderr,
            duration_ms: result.stats.duration.as_secs_f64() * 1000.0,
            callback_invocations: result.stats.callback_invocations,
            peak_memory_bytes: result.stats.peak_memory_bytes,
            fuel_consumed: result.stats.fuel_consumed,
            result_json: result.result,
            result_error: result.result_error,
        }
    }
}

impl ExecuteResult {
    /// Create an ExecuteResult from ExecutionOutput (used by Session).
    pub(crate) fn from_execution_output(output: eryx::ExecutionOutput) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms: output.duration.as_secs_f64() * 1000.0,
            callback_invocations: output.callback_invocations,
            peak_memory_bytes: Some(output.peak_memory_bytes),
            fuel_consumed: output.fuel_consumed,
            result_json: output.result,
            result_error: output.result_error,
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
