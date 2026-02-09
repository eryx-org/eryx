//! Session wrapper for Python.
//!
//! Provides the `Session` class that maintains persistent Python state across
//! multiple executions, with optional VFS support.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eryx::Callback;
use eryx::OutputHandler;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use tokio::sync::mpsc;

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::result::ExecuteResult;
use crate::sandbox::PyOutputHandler;
use crate::vfs::VfsStorage;

/// A session that maintains persistent Python state across executions.
///
/// Unlike `Sandbox` which runs each execution in isolation, `Session` preserves
/// Python variables, functions, and classes between `execute()` calls. This is
/// useful for:
///
/// - Interactive REPL-style execution
/// - Building up state incrementally
/// - Faster subsequent executions (no Python initialization overhead)
///
/// Sessions can optionally use a virtual filesystem (VFS) for persistent file
/// storage that survives across executions and even session resets.
///
/// Example:
///     # Basic session usage
///     session = Session()
///     session.execute('x = 1')
///     session.execute('y = 2')
///     result = session.execute('print(x + y)')
///     print(result.stdout)  # "3"
///
///     # Session with VFS for file persistence
///     storage = VfsStorage()
///     session = Session(vfs=storage)
///     session.execute('open("/data/test.txt", "w").write("hello")')
///     result = session.execute('print(open("/data/test.txt").read())')
///     print(result.stdout)  # "hello"
#[pyclass(module = "eryx")]
pub struct Session {
    /// The underlying SessionExecutor wrapped in Mutex for thread safety.
    /// SessionExecutor is Send but not Sync (due to wasmtime internals),
    /// so we use Mutex to provide the Sync guarantee required by PyO3.
    inner: Mutex<Option<eryx::SessionExecutor>>,
    /// The PythonExecutor that backs this session.
    /// Kept for potential future use (e.g., reset with new imports).
    #[allow(dead_code)]
    executor: Arc<eryx::PythonExecutor>,
    /// Tokio runtime for executing async code.
    runtime: Arc<tokio::runtime::Runtime>,
    /// VFS storage (kept for sharing across sessions).
    vfs_storage: Option<Arc<eryx::vfs::ScrubbingStorage<eryx::vfs::InMemoryStorage>>>,
    /// VFS mount path configuration.
    vfs_mount_path: Option<String>,
    /// Callbacks available for this session.
    callbacks: Arc<HashMap<String, Arc<dyn eryx::Callback>>>,
    /// Output handler for streaming stdout/stderr.
    output_handler: Option<Arc<dyn OutputHandler>>,
}

#[pymethods]
impl Session {
    /// Create a new session with the embedded Python runtime.
    ///
    /// Sessions maintain persistent Python state across `execute()` calls,
    /// unlike `Sandbox` which runs each execution in isolation.
    ///
    /// Args:
    ///     vfs: Optional VfsStorage for persistent file storage.
    ///         Files written to `/data/*` will persist across executions.
    ///     vfs_mount_path: Custom mount path for VFS (default: "/data").
    ///     execution_timeout_ms: Optional timeout in milliseconds for each execution.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
    ///         Can be a CallbackRegistry or a list of callback dicts.
    ///
    /// Returns:
    ///     A new Session instance ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If the session fails to initialize.
    ///
    /// Example:
    ///     # Basic session
    ///     session = Session()
    ///     session.execute('x = 42')
    ///     result = session.execute('print(x)')  # prints "42"
    ///
    ///     # Session with VFS
    ///     storage = VfsStorage()
    ///     session = Session(vfs=storage)
    ///     session.execute('open("/data/file.txt", "w").write("data")')
    ///
    ///     # Session with callbacks
    ///     def get_time():
    ///         import time
    ///         return {"timestamp": time.time()}
    ///
    ///     session = Session(callbacks=[
    ///         {"name": "get_time", "fn": get_time, "description": "Returns current time"}
    ///     ])
    #[new]
    #[pyo3(signature = (*, vfs=None, vfs_mount_path=None, execution_timeout_ms=None, callbacks=None, mcp=None, volumes=None, on_stdout=None, on_stderr=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        vfs: Option<VfsStorage>,
        vfs_mount_path: Option<String>,
        execution_timeout_ms: Option<u64>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        volumes: Option<Vec<(String, String, bool)>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        // Create a tokio runtime for async execution
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );

        // Create the PythonExecutor from embedded runtime
        let executor = Arc::new(eryx::PythonExecutor::from_embedded_runtime().map_err(|e| {
            InitializationError::new_err(format!("failed to create executor: {e}"))
        })?);

        // Extract callbacks if provided
        let callbacks_map: Arc<HashMap<String, Arc<dyn eryx::Callback>>> = {
            let mut map: HashMap<String, Arc<dyn eryx::Callback>> = if let Some(ref cbs) = callbacks
            {
                let python_callbacks = extract_callbacks(py, cbs)?;
                python_callbacks
                    .into_iter()
                    .map(|c| (c.name().to_string(), Arc::new(c) as Arc<dyn eryx::Callback>))
                    .collect()
            } else {
                HashMap::new()
            };

            // Merge MCP callbacks if provided
            if let Some(ref mcp_mgr) = mcp {
                for c in mcp_mgr.as_callbacks() {
                    let arc: Arc<dyn eryx::Callback> = Arc::new(c);
                    map.insert(arc.name().to_string(), arc);
                }
            }

            Arc::new(map)
        };

        // Convert to slice for SessionExecutor
        let callbacks_vec: Vec<Arc<dyn eryx::Callback>> = callbacks_map.values().cloned().collect();

        // Convert volume tuples to VolumeMount structs
        let volume_mounts: Vec<eryx::VolumeMount> = volumes
            .unwrap_or_default()
            .into_iter()
            .map(|(host_path, guest_path, read_only)| {
                if read_only {
                    eryx::VolumeMount::read_only(host_path, guest_path)
                } else {
                    eryx::VolumeMount::new(host_path, guest_path)
                }
            })
            .collect();

        // Create the SessionExecutor
        let vfs_storage = vfs.map(|v| v.inner);
        let mount_path = vfs_mount_path.clone();
        let needs_vfs = vfs_storage.is_some() || !volume_mounts.is_empty();

        let (inner, vfs_storage) = runtime
            .block_on(async {
                if needs_vfs {
                    // Auto-create VFS storage if volumes are requested but no VFS provided
                    let storage = vfs_storage.unwrap_or_else(|| {
                        Arc::new(eryx::vfs::ScrubbingStorage::new(
                            eryx::vfs::InMemoryStorage::new(),
                            std::collections::HashMap::new(),
                            eryx::vfs::VfsFileScrubPolicy::None,
                        ))
                    });
                    let mut config = if let Some(path) = &mount_path {
                        eryx::VfsConfig::new(path)
                    } else {
                        eryx::VfsConfig::default()
                    };
                    config.volumes = volume_mounts;
                    let session = eryx::SessionExecutor::new_with_vfs_config(
                        Arc::clone(&executor),
                        &callbacks_vec,
                        Arc::clone(&storage),
                        config,
                    )
                    .await?;
                    Ok((session, Some(storage)))
                } else {
                    let session =
                        eryx::SessionExecutor::new(Arc::clone(&executor), &callbacks_vec).await?;
                    Ok((session, None))
                }
            })
            .map_err(eryx_error_to_py)?;

        // Build output handler if streaming callbacks are provided
        let output_handler: Option<Arc<dyn OutputHandler>> =
            if on_stdout.is_some() || on_stderr.is_some() {
                Some(Arc::new(PyOutputHandler {
                    on_stdout,
                    on_stderr,
                }))
            } else {
                None
            };

        let session = Self {
            inner: Mutex::new(Some(inner)),
            executor,
            runtime,
            vfs_storage,
            vfs_mount_path: mount_path,
            callbacks: callbacks_map,
            output_handler,
        };

        // Set execution timeout if provided
        if let Some(timeout_ms) = execution_timeout_ms {
            session.set_execution_timeout_ms(Some(timeout_ms))?;
        }

        Ok(session)
    }

    /// Execute Python code in the session.
    ///
    /// Unlike `Sandbox.execute()`, state from previous executions is preserved.
    /// Variables, functions, and classes defined in one call are available in
    /// subsequent calls.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///
    /// Returns:
    ///     ExecuteResult containing stdout and execution statistics.
    ///
    /// Raises:
    ///     ExecutionError: If the Python code raises an exception.
    ///     TimeoutError: If execution exceeds the timeout limit.
    ///
    /// Example:
    ///     session.execute('x = 1')
    ///     session.execute('y = 2')
    ///     result = session.execute('print(x + y)')
    ///     print(result.stdout)  # "3"
    fn execute(&self, py: Python<'_>, code: &str) -> PyResult<ExecuteResult> {
        let code = code.to_string();
        let runtime = self.runtime.clone();
        let callbacks_map = self.callbacks.clone();
        let output_handler = self.output_handler.clone();

        // Release the GIL while executing
        py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            // Get callbacks as a vec for with_callbacks
            let callbacks_vec: Vec<Arc<dyn eryx::Callback>> =
                callbacks_map.values().cloned().collect();

            runtime
                .block_on(async {
                    // Create callback channel
                    let (callback_tx, callback_rx) = tokio::sync::mpsc::channel(32);

                    // Spawn callback handler task
                    let handler_callbacks = callbacks_map.clone();
                    let handler = tokio::spawn(async move {
                        eryx::callback_handler::run_callback_handler(
                            callback_rx,
                            handler_callbacks,
                            eryx::ResourceLimits::default(),
                            std::sync::Arc::new(std::collections::HashMap::new()),
                        )
                        .await
                    });

                    // Spawn output collector for real-time streaming if handler is configured
                    let (output_tx, output_collector) = if output_handler.is_some() {
                        let (tx, rx) = mpsc::unbounded_channel::<eryx::OutputRequest>();
                        let handler = output_handler.clone();
                        let task = tokio::spawn(async move {
                            eryx::callback_handler::run_output_collector(
                                rx,
                                handler,
                                std::collections::HashMap::new(),
                                false,
                                false,
                            )
                            .await;
                        });
                        (Some(tx), Some(task))
                    } else {
                        (None, None)
                    };

                    // Execute with callbacks and optional output streaming
                    let mut builder = inner
                        .execute(&code)
                        .with_callbacks(&callbacks_vec, callback_tx);

                    if let Some(tx) = output_tx {
                        builder = builder.with_output_streaming(tx);
                    }

                    let result = builder.run().await;

                    // Wait for handler to finish (it will exit when channel closes)
                    let _callback_count = handler.await.unwrap_or(0);

                    // Wait for output collector to finish
                    if let Some(collector) = output_collector {
                        let _ = collector.await;
                    }

                    result
                })
                .map(ExecuteResult::from_execution_output)
                .map_err(eryx_error_to_py)
        })
    }

    /// Reset the session to a fresh state.
    ///
    /// This clears all Python variables and state, but VFS storage persists
    /// if it was provided at session creation.
    ///
    /// Example:
    ///     session.execute('x = 42')
    ///     session.reset()
    ///     # x is no longer defined
    ///     session.execute('print(x)')  # raises NameError
    fn reset(&self, py: Python<'_>) -> PyResult<()> {
        let runtime = self.runtime.clone();

        py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            runtime.block_on(inner.reset(&[])).map_err(eryx_error_to_py)
        })
    }

    /// Clear Python state without fully resetting the session.
    ///
    /// This is lighter-weight than `reset()` - it clears Python variables
    /// but doesn't recreate the WASM instance.
    ///
    /// Example:
    ///     session.execute('x = 42')
    ///     session.clear_state()
    ///     # x is no longer defined
    fn clear_state(&self, py: Python<'_>) -> PyResult<()> {
        let runtime = self.runtime.clone();

        py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            runtime
                .block_on(inner.clear_state())
                .map_err(eryx_error_to_py)
        })
    }

    /// Capture a snapshot of the current Python state.
    ///
    /// The snapshot contains all user-defined variables, serialized using pickle.
    /// It can be saved to disk and restored later.
    ///
    /// Returns:
    ///     bytes: The serialized snapshot data.
    ///
    /// Raises:
    ///     ExecutionError: If the state cannot be serialized.
    ///
    /// Example:
    ///     session.execute('x = 42')
    ///     snapshot = session.snapshot_state()
    ///     # Save snapshot to file
    ///     with open('state.bin', 'wb') as f:
    ///         f.write(snapshot)
    fn snapshot_state<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let runtime = self.runtime.clone();

        let snapshot = py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            runtime
                .block_on(inner.snapshot_state())
                .map_err(eryx_error_to_py)
        })?;

        Ok(PyBytes::new(py, snapshot.to_bytes().as_slice()))
    }

    /// Restore Python state from a previously captured snapshot.
    ///
    /// Args:
    ///     snapshot: The serialized snapshot data (bytes).
    ///
    /// Raises:
    ///     ExecutionError: If the snapshot cannot be restored.
    ///
    /// Example:
    ///     # Load snapshot from file
    ///     with open('state.bin', 'rb') as f:
    ///         snapshot = f.read()
    ///     session.restore_state(snapshot)
    ///     result = session.execute('print(x)')  # x was in the snapshot
    fn restore_state(&self, py: Python<'_>, snapshot: &[u8]) -> PyResult<()> {
        let runtime = self.runtime.clone();
        let snapshot = eryx::PythonStateSnapshot::from_bytes(snapshot).map_err(eryx_error_to_py)?;

        py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            runtime
                .block_on(inner.restore_state(&snapshot))
                .map_err(eryx_error_to_py)
        })
    }

    /// Get the number of executions performed in this session.
    #[getter]
    fn execution_count(&self) -> PyResult<u32> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
        let inner = guard
            .as_ref()
            .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;
        Ok(inner.execution_count())
    }

    /// Get the current execution timeout in milliseconds, or None if not set.
    #[getter]
    fn execution_timeout_ms(&self) -> PyResult<Option<u64>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
        let inner = guard
            .as_ref()
            .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;
        Ok(inner.execution_timeout().map(|d| d.as_millis() as u64))
    }

    /// Set the execution timeout.
    ///
    /// Args:
    ///     timeout_ms: Timeout in milliseconds, or None to disable.
    #[setter]
    fn set_execution_timeout_ms(&self, timeout_ms: Option<u64>) -> PyResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| InitializationError::new_err("session lock poisoned"))?;
        let inner = guard
            .as_mut()
            .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;
        let timeout = timeout_ms.map(Duration::from_millis);
        inner.set_execution_timeout(timeout);
        Ok(())
    }

    /// Get the VFS storage used by this session, if any.
    #[getter]
    fn vfs(&self) -> Option<VfsStorage> {
        self.vfs_storage.as_ref().map(|storage| VfsStorage {
            inner: Arc::clone(storage),
        })
    }

    /// Get the VFS mount path, if VFS is enabled.
    #[getter]
    fn vfs_mount_path(&self) -> Option<String> {
        if self.vfs_storage.is_some() {
            Some(
                self.vfs_mount_path
                    .clone()
                    .unwrap_or_else(|| "/data".to_string()),
            )
        } else {
            None
        }
    }

    fn __repr__(&self) -> String {
        let count = self
            .inner
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|i| i.execution_count()))
            .unwrap_or(0);
        let vfs_info = if self.vfs_storage.is_some() {
            let path = self.vfs_mount_path.as_deref().unwrap_or("/data");
            format!(", vfs_mount_path={:?}", path)
        } else {
            String::new()
        };
        format!("Session(execution_count={}{})", count, vfs_info)
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let execution_count = self
            .inner
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|i| i.execution_count()));
        f.debug_struct("Session")
            .field("execution_count", &execution_count)
            .field("has_vfs", &self.vfs_storage.is_some())
            .finish_non_exhaustive()
    }
}

// Static assertions that Session is Send + Sync (required for PyO3 thread safety)
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Session>();
};
