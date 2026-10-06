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
    /// Complete stdout output from the sandboxed code (raw bytes).
    pub stdout: Vec<u8>,

    /// Complete stderr output from the sandboxed code (raw bytes).
    pub stderr: Vec<u8>,

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
    /// stdout as raw bytes.
    #[getter]
    fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// stderr as raw bytes.
    #[getter]
    fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// stdout decoded as UTF-8, replacing invalid sequences with U+FFFD.
    #[getter]
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr decoded as UTF-8, replacing invalid sequences with U+FFFD.
    #[getter]
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

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
        let stdout_str = String::from_utf8_lossy(&self.stdout);
        let stderr_str = String::from_utf8_lossy(&self.stderr);
        format!(
            "ExecuteResult(stdout={:?}, stderr={:?}, duration_ms={:.2}, callback_invocations={}, peak_memory_bytes={:?}, fuel_consumed={:?}, result={:?}, result_error={:?})",
            truncate_string(&stdout_str, 50),
            truncate_string(&stderr_str, 50),
            self.duration_ms,
            self.callback_invocations,
            self.peak_memory_bytes,
            self.fuel_consumed,
            self.result_json.as_deref().map(|s| truncate_string(s, 50)),
            self.result_error,
        )
    }

    fn __str__(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
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

/// Result of `Sandbox.execute_with_journal()`.
///
/// Exactly one of `result` and `error` is set. `journal` is always present,
/// even when execution failed or suspended, so a later run can replay every
/// callback that completed. When `suspended` is set, `error` holds the
/// resulting `ExecutionError`; branch on `suspended` first.
#[pyclass(frozen, module = "eryx")]
#[derive(Debug)]
pub struct ReplayOutcome {
    /// The execution result, or `None` if execution failed or suspended.
    #[pyo3(get)]
    result: Option<ExecuteResult>,

    /// The exception `execute()` would have raised, or `None` on success.
    #[pyo3(get)]
    error: Option<Py<PyAny>>,

    /// The callback journal recorded during this run, as a JSON-compatible
    /// dict. Pass it as `replay_journal=` to a new `Sandbox` to replay it.
    #[pyo3(get)]
    journal: Py<PyAny>,

    /// How many callbacks were served from the replay journal.
    #[pyo3(get)]
    replayed_callbacks: u32,

    /// The callback that suspended execution, or `None`.
    #[pyo3(get)]
    suspended: Option<SuspendedCallback>,
}

#[pymethods]
impl ReplayOutcome {
    fn __repr__(&self, py: Python<'_>) -> String {
        let entries = self
            .journal
            .bind(py)
            .get_item("entries")
            .map_or(0, |e| e.len().unwrap_or(0));
        format!(
            "ReplayOutcome(ok={}, journal_entries={}, replayed_callbacks={}, suspended={:?})",
            self.error.is_none(),
            entries,
            self.replayed_callbacks,
            self.suspended.as_ref().map(|s| &s.name),
        )
    }
}

impl ReplayOutcome {
    /// Convert an `eryx::ReplayOutcome`, mapping any error to its Python exception.
    pub(crate) fn from_outcome(py: Python<'_>, outcome: eryx::ReplayOutcome) -> PyResult<Self> {
        let (result, error) = match outcome.result {
            Ok(r) => (Some(ExecuteResult::from(r)), None),
            Err(e) => (
                None,
                Some(crate::error::eryx_error_to_py(e).into_value(py).into_any()),
            ),
        };
        let journal = pythonize::pythonize(py, &outcome.journal)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?
            .unbind();
        Ok(Self {
            result,
            error,
            journal,
            replayed_callbacks: outcome.replayed_callbacks,
            suspended: outcome.suspended.map(|s| SuspendedCallback {
                name: s.name,
                args_json: s.args_json,
                reason: s.reason,
            }),
        })
    }
}

/// Details of the callback that suspended execution by raising `SuspendCallback`.
#[pyclass(frozen, module = "eryx", from_py_object)]
#[derive(Debug, Clone)]
pub struct SuspendedCallback {
    /// Name of the callback that suspended.
    #[pyo3(get)]
    pub name: String,

    /// Canonicalized JSON arguments the callback was invoked with.
    #[pyo3(get)]
    pub args_json: String,

    /// The reason string passed to `SuspendCallback`.
    #[pyo3(get)]
    pub reason: String,
}

#[pymethods]
impl SuspendedCallback {
    fn __repr__(&self) -> String {
        format!(
            "SuspendedCallback(name={:?}, args_json={:?}, reason={:?})",
            self.name, self.args_json, self.reason
        )
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
