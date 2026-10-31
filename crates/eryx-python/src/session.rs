//! Session wrapper for Python.
//!
//! Provides the `Session` class that maintains persistent Python state across
//! multiple executions, with optional VFS support.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eryx::Callback;
use eryx::{CancellationToken, OutputHandler};
use pyo3::prelude::*;
use tokio::sync::mpsc;

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::execution::{CancelToken, call_token, run_async, run_blocking};
use crate::net_config::NetConfig;
use crate::resource_limits::ResourceLimits;
use crate::result::{ExecuteResult, ReplayOutcome, StateSnapshot};
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
/// `snapshot_state()`, `restore_state()` and `clear_state()` run under the
/// execution timeout raised to at least 5 seconds, or 30 seconds when the
/// session has no execution timeout. That state-operation bound can't be set
/// separately from Python yet.
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
    /// so we use Mutex to provide the Sync guarantee required by PyO3. It is
    /// an async mutex so `execute_async()` can wait for it on the runtime.
    inner: Arc<tokio::sync::Mutex<Option<eryx::SessionExecutor>>>,
    /// Completes when the most recently queued call finishes. Executions and
    /// `reset`/`clear_state`/`snapshot_state`/`restore_state` take their
    /// place here when called, so they run in call order.
    queue_tail: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// Tokio runtime for executing async code.
    runtime: Arc<tokio::runtime::Runtime>,
    /// VFS storage (kept for sharing across sessions).
    vfs_storage: Option<eryx::vfs::ArcStorage>,
    /// VFS mount path configuration.
    vfs_mount_path: Option<String>,
    /// Callbacks available for this session.
    callbacks: Arc<HashMap<String, Arc<dyn eryx::Callback>>>,
    /// Network configuration for this session.
    net_config: Option<eryx::NetConfig>,
    /// Output handler for streaming stdout/stderr.
    output_handler: Option<Arc<dyn OutputHandler>>,
    /// Keep extracted package directories alive for factory-created sessions.
    _package_storage_owner: Option<Arc<Vec<eryx::ExtractedPackage>>>,
    /// Limits passed to callback handlers for this session.
    callback_limits: eryx::ResourceLimits,
}

impl Session {
    /// Build a session from a preconfigured executor. Factory sessions use this
    /// path so ordinary `Session` construction remains unchanged.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_executor(
        py: Python<'_>,
        executor: Arc<eryx::PythonExecutor>,
        runtime: Arc<tokio::runtime::Runtime>,
        vfs: Option<VfsStorage>,
        vfs_mount_path: Option<String>,
        limits: eryx::ResourceLimits,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        volumes: Option<Vec<crate::vfs::VolumeSpec>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
        package_storage_owner: Option<Arc<Vec<eryx::ExtractedPackage>>>,
    ) -> PyResult<Self> {
        let net_config: Option<eryx::NetConfig> = network.map(Into::into);
        if let Some(net) = &net_config {
            net.validate().map_err(InitializationError::new_err)?;
        }
        let callbacks_map: Arc<HashMap<String, Arc<dyn eryx::Callback>>> = {
            let mut map = if let Some(ref cbs) = callbacks {
                extract_callbacks(py, cbs)?
                    .into_iter()
                    .map(|c| (c.name().to_string(), Arc::new(c) as Arc<dyn eryx::Callback>))
                    .collect()
            } else {
                HashMap::new()
            };
            if let Some(ref mcp_mgr) = mcp {
                for c in mcp_mgr.as_callbacks() {
                    let arc: Arc<dyn eryx::Callback> = Arc::new(c);
                    map.insert(arc.name().to_string(), arc);
                }
            }
            Arc::new(map)
        };
        let callbacks_vec: Vec<Arc<dyn eryx::Callback>> = callbacks_map.values().cloned().collect();
        let volume_mounts = volumes
            .unwrap_or_default()
            .into_iter()
            .map(eryx::VolumeMount::from)
            .collect::<Vec<_>>();
        let vfs_storage = vfs.map(VfsStorage::into_arc_storage);
        let mount_path = vfs_mount_path.clone();
        let needs_vfs = vfs_storage.is_some() || !volume_mounts.is_empty();
        let (inner, vfs_storage) = runtime
            .block_on(async {
                if needs_vfs {
                    let storage = vfs_storage.unwrap_or_else(|| {
                        let storage = match limits.max_vfs_bytes {
                            Some(max_bytes) => {
                                eryx::vfs::InMemoryStorage::with_max_bytes(max_bytes)
                            }
                            None => eryx::vfs::InMemoryStorage::new(),
                        };
                        eryx::vfs::ArcStorage::new(Arc::new(eryx::vfs::ScrubbingStorage::new(
                            storage,
                            HashMap::new(),
                            eryx::vfs::VfsFileScrubPolicy::None,
                        )))
                    });
                    let mut config = if let Some(path) = &mount_path {
                        eryx::VfsConfig::new(path)
                    } else {
                        eryx::VfsConfig::default()
                    };
                    config.volumes = volume_mounts;
                    let session = eryx::SessionExecutor::new_with_vfs_config_and_limits(
                        Arc::clone(&executor),
                        &callbacks_vec,
                        storage.clone(),
                        config,
                        &limits,
                    )
                    .await?;
                    Ok((session, Some(storage)))
                } else {
                    let session = eryx::SessionExecutor::new_with_limits(
                        Arc::clone(&executor),
                        &callbacks_vec,
                        &limits,
                    )
                    .await?;
                    Ok((session, None))
                }
            })
            .map_err(eryx_error_to_py)?;
        let callback_limits = eryx::ResourceLimits::unlimited()
            .with_callback_timeout(limits.callback_timeout)
            .with_max_callback_invocations(limits.max_callback_invocations);
        let output_handler = if on_stdout.is_some() || on_stderr.is_some() {
            Some(Arc::new(PyOutputHandler {
                on_stdout,
                on_stderr,
            }) as Arc<dyn OutputHandler>)
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(tokio::sync::Mutex::new(Some(inner))),
            queue_tail: Mutex::new(None),
            runtime,
            vfs_storage,
            vfs_mount_path: mount_path,
            callbacks: callbacks_map,
            net_config,
            output_handler,
            _package_storage_owner: package_storage_owner,
            callback_limits,
        })
    }
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
    ///     resource_limits: Optional limits for each execution. Defaults to
    ///         `ResourceLimits()` (30-second execution timeout, 128 MB memory).
    ///     execution_timeout_ms: Overrides `resource_limits.execution_timeout_ms`.
    ///     max_fuel: Overrides `resource_limits.max_fuel`.
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
    #[pyo3(signature = (*, vfs=None, vfs_mount_path=None, resource_limits=None, execution_timeout_ms=None, max_fuel=None, network=None, callbacks=None, mcp=None, volumes=None, on_stdout=None, on_stderr=None, result_variable=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        vfs: Option<VfsStorage>,
        vfs_mount_path: Option<String>,
        resource_limits: Option<ResourceLimits>,
        execution_timeout_ms: Option<u64>,
        max_fuel: Option<u64>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        volumes: Option<Vec<crate::vfs::VolumeSpec>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
        result_variable: Option<String>,
    ) -> PyResult<Self> {
        let runtime = crate::error::make_runtime()?;

        // Create the PythonExecutor from embedded runtime
        let mut executor = eryx::PythonExecutor::from_embedded_runtime()
            .map_err(|e| InitializationError::new_err(format!("failed to create executor: {e}")))?;
        if let Some(name) = result_variable {
            executor = executor.with_result_variable(name);
        }
        let executor = Arc::new(executor);

        let mut limits = resource_limits.unwrap_or_default();
        if execution_timeout_ms.is_some() {
            limits.execution_timeout_ms = execution_timeout_ms;
        }
        if max_fuel.is_some() {
            limits.max_fuel = max_fuel;
        }
        Self::from_executor(
            py,
            executor,
            runtime,
            vfs,
            vfs_mount_path,
            (&limits).into(),
            network,
            callbacks,
            mcp,
            volumes,
            on_stdout,
            on_stderr,
            None,
        )
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
    ///     CancelledError: If `cancel` was cancelled.
    ///
    /// The GIL is released while the code runs. Ctrl-C (or any signal handler
    /// that raises) stops the run and re-raises the handler's exception.
    ///
    /// A cancelled or interrupted run loses the session's Python state: the
    /// next call starts from a fresh interpreter (VFS files are kept).
    ///
    /// Example:
    ///     session.execute('x = 1')
    ///     session.execute('y = 2')
    ///     result = session.execute('print(x + y)')
    ///     print(result.stdout)  # "3"
    #[pyo3(signature = (code, *, cancel=None))]
    fn execute(
        &self,
        py: Python<'_>,
        code: &str,
        cancel: Option<&CancelToken>,
    ) -> PyResult<ExecuteResult> {
        let token = call_token(cancel);
        let run = self.run(code, self.callbacks.clone(), None, token.clone());
        flatten_run(run_blocking(py, &self.runtime, &token, run)??)
    }

    /// Execute code in the session from asyncio.
    ///
    /// Like `execute()`, but returns an awaitable so the event loop keeps
    /// running while the code executes. Concurrent calls on one session run
    /// one at a time, in call order, and `reset()`, `clear_state()`,
    /// `snapshot_state()` and `restore_state()` wait behind calls made before
    /// them. Cancelling the awaiting task stops the run (losing session state,
    /// as with `cancel`).
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///     cancel: Optional CancelToken; cancelling it stops the run.
    ///
    /// Returns:
    ///     An awaitable resolving to an ExecuteResult.
    ///
    /// Example:
    ///     await session.execute_async('x = 1')
    ///     result = await session.execute_async('print(x)')
    #[pyo3(signature = (code, *, cancel=None))]
    fn execute_async<'py>(
        &self,
        py: Python<'py>,
        code: &str,
        cancel: Option<&CancelToken>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let token = call_token(cancel);
        let run_token = token.clone();
        run_async(py, &self.runtime, token, || {
            let run = self.run(code, self.callbacks.clone(), None, run_token);
            async move { flatten_run(run.await?) }
        })
    }

    /// Execute code in the session, journaling callback results and replaying
    /// them from `journal` if one was given. Session state persists as with
    /// `execute()`. See `Sandbox.execute_with_journal()`.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///     journal: Optional journal from a previous `ReplayOutcome.journal`.
    ///         Trusted input: replayed results are returned verbatim.
    ///
    /// If the run halts (a callback suspends, or it times out or runs out of
    /// fuel), the session is rolled back to its state from before the call, so
    /// resuming in the same session does not apply the replayed prefix's
    /// effects on Python globals twice. See the `.pyi` docstring for details.
    ///
    /// Returns:
    ///     ReplayOutcome with the result or error, the recorded journal, the
    ///     number of replayed callbacks, and any suspension details.
    ///
    /// Raises:
    ///     ValueError: If `journal` is not a valid journal.
    #[pyo3(signature = (code, journal=None))]
    fn execute_with_journal(
        &self,
        py: Python<'_>,
        code: &str,
        journal: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<ReplayOutcome> {
        let journal = crate::result::journal_from_py(journal)?;
        let state = eryx::ReplayState::shared(code, journal);
        let wrapped = eryx::replay::wrap_callbacks(&self.callbacks, &state);
        let token = call_token(None);
        let run = self.run(code, Arc::new(wrapped), Some(state.clone()), token.clone());
        match run_blocking(py, &self.runtime, &token, run)?? {
            Ok(result) => ReplayOutcome::from_state(
                py,
                result.map(ExecuteResult::from_execution_output),
                &state,
                code,
            ),
            Err(rollback_error) => {
                let error = InitializationError::new_err(format!(
                    "session could not be rolled back after the run halted and is unusable; \
                     resume with the journal in a new session: {rollback_error}"
                ));
                Ok(
                    ReplayOutcome::from_state(py, Err(rollback_error), &state, code)?
                        .into_rollback_failure(py, error),
                )
            }
        }
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
        self.queued(py, async |inner| inner.reset(&[]).await)
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
        self.queued(py, async |inner| inner.clear_state().await)
    }

    /// Capture a snapshot of the current Python state.
    ///
    /// The snapshot contains all user-defined variables, serialized using pickle.
    /// It can be saved to disk and restored later. Variables that can't be
    /// serialized are left out, and named in `skipped_globals`.
    ///
    /// Returns:
    ///     StateSnapshot: The serialized state (`data`) and the names of the
    ///     globals left out (`skipped_globals`).
    ///
    /// Raises:
    ///     ExecutionError: If the state cannot be serialized.
    ///
    /// Example:
    ///     session.execute('x = 42')
    ///     snapshot = session.snapshot_state()
    ///     # Save snapshot to file
    ///     with open('state.bin', 'wb') as f:
    ///         f.write(snapshot.data)
    fn snapshot_state(&self, py: Python<'_>) -> PyResult<StateSnapshot> {
        let snapshot = self.queued(py, async |inner| inner.snapshot_state().await)?;

        Ok(StateSnapshot {
            data: snapshot.to_bytes(),
            skipped_globals: snapshot.skipped_globals().to_vec(),
        })
    }

    /// Restore Python state from a previously captured snapshot.
    ///
    /// Args:
    ///     snapshot: A `StateSnapshot`, or its `data` bytes.
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
    fn restore_state(&self, py: Python<'_>, snapshot: &Bound<'_, PyAny>) -> PyResult<()> {
        let snapshot = match snapshot.cast::<StateSnapshot>() {
            Ok(snapshot) => eryx::PythonStateSnapshot::from_bytes(&snapshot.get().data),
            Err(_) => eryx::PythonStateSnapshot::from_bytes(snapshot.extract::<&[u8]>()?),
        }
        .map_err(eryx_error_to_py)?;
        self.queued(py, async |inner| inner.restore_state(&snapshot).await)
    }

    /// Get the number of executions performed in this session.
    #[getter]
    fn execution_count(&self, py: Python<'_>) -> PyResult<u32> {
        py.detach(|| self.with_inner(|inner| inner.execution_count()))
    }

    /// Get the current execution timeout in milliseconds, or None if not set.
    #[getter]
    fn execution_timeout_ms(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        py.detach(|| {
            self.with_inner(|inner| inner.execution_timeout().map(|d| d.as_millis() as u64))
        })
    }

    /// Set the execution timeout.
    ///
    /// Args:
    ///     timeout_ms: Timeout in milliseconds, or None to disable.
    #[setter]
    fn set_execution_timeout_ms(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<()> {
        let timeout = timeout_ms.map(Duration::from_millis);
        py.detach(|| self.with_inner(|inner| inner.set_execution_timeout(timeout)))
    }

    /// Get the current fuel limit, or None if not set.
    #[getter]
    fn fuel_limit(&self, py: Python<'_>) -> PyResult<Option<u64>> {
        py.detach(|| self.with_inner(|inner| inner.fuel_limit()))
    }

    /// Set the fuel limit (max WASM instructions per execution).
    ///
    /// Args:
    ///     limit: Fuel limit, or None to disable (fuel is still tracked).
    #[setter]
    fn set_fuel_limit(&self, py: Python<'_>, limit: Option<u64>) -> PyResult<()> {
        py.detach(|| self.with_inner(|inner| inner.set_fuel_limit(limit)))
    }

    /// Get the VFS storage used by this session, if any.
    #[getter]
    fn vfs(&self) -> Option<VfsStorage> {
        self.vfs_storage.as_ref().map(|storage| VfsStorage {
            inner: storage.clone(),
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
        // Don't wait for the lock: another thread may be mid-execution, and
        // that execution may need the GIL this thread holds.
        let count = match self.inner.try_lock() {
            Ok(guard) => guard
                .as_ref()
                .map_or(0, |i| i.execution_count())
                .to_string(),
            Err(_) => "<busy>".to_string(),
        };
        let vfs_info = if self.vfs_storage.is_some() {
            let path = self.vfs_mount_path.as_deref().unwrap_or("/data");
            format!(", vfs_mount_path={:?}", path)
        } else {
            String::new()
        };
        format!("Session(execution_count={}{})", count, vfs_info)
    }
}

impl Session {
    /// Take the next place in the call-order queue.
    fn queue_slot(&self) -> QueueSlot {
        let (done, queued) = tokio::sync::oneshot::channel();
        let previous = self
            .queue_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(queued);
        QueueSlot { previous, done }
    }

    /// Run `f` on the session executor once every call queued before it has
    /// finished, with the GIL released.
    fn queued<T: Send>(
        &self,
        py: Python<'_>,
        f: impl AsyncFnOnce(&mut eryx::SessionExecutor) -> Result<T, eryx::Error> + Send,
    ) -> PyResult<T> {
        let slot = self.queue_slot();
        py.detach(|| {
            self.runtime.block_on(async {
                let _done = slot.wait().await;
                let mut guard = self.inner.lock().await;
                let inner = guard
                    .as_mut()
                    .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;
                f(inner).await.map_err(eryx_error_to_py)
            })
        })
    }

    /// Run `f` on the session executor under its lock.
    ///
    /// `execute()` holds the lock with the GIL released while its callbacks and
    /// output handlers re-acquire the GIL, so callers holding the GIL must call
    /// this inside `py.detach` or they can deadlock against a running execution.
    fn with_inner<T>(&self, f: impl FnOnce(&mut eryx::SessionExecutor) -> T) -> PyResult<T> {
        let mut guard = self.inner.blocking_lock();
        let inner = guard
            .as_mut()
            .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;
        Ok(f(inner))
    }

    /// Shared body of `execute()`, `execute_async()` and
    /// `execute_with_journal()`, as a future that owns what it needs so it can
    /// run as its own task. Cancelling `token` stops the run.
    ///
    /// The `PyResult` carries session-level failures before the run. With
    /// `replay` set (a journaled call), the run goes through
    /// `SessionExecutor::run_with_rollback`, and the middle `Err` means that
    /// rollback failed; the inner `Result` is the execution outcome.
    fn run(
        &self,
        code: &str,
        callbacks_map: Arc<HashMap<String, Arc<dyn eryx::Callback>>>,
        replay: Option<Arc<Mutex<eryx::ReplayState>>>,
        token: CancellationToken,
    ) -> impl Future<Output = PyResult<RunOutcome>> + Send + 'static {
        let code = code.to_string();
        let session = Arc::clone(&self.inner);
        let output_handler = self.output_handler.clone();
        let net_config = self.net_config.clone();
        let callback_limits = self.callback_limits.clone();
        let slot = self.queue_slot();

        async move {
            let _done = slot.wait().await;
            let mut guard = session.lock_owned().await;
            let inner = guard
                .as_mut()
                .ok_or_else(|| InitializationError::new_err("session is not initialized"))?;

            // Get callbacks as a vec for with_callbacks
            let callbacks_vec: Vec<Arc<dyn eryx::Callback>> =
                callbacks_map.values().cloned().collect();

            let body = async |inner: &mut eryx::SessionExecutor| {
                // Create callback channel
                let (callback_tx, callback_rx) = tokio::sync::mpsc::channel(32);

                // Spawn callback handler task
                let handler_callbacks = callbacks_map.clone();
                let handler = tokio::spawn(async move {
                    eryx::callback_handler::run_callback_handler(
                        callback_rx,
                        handler_callbacks,
                        callback_limits,
                        std::sync::Arc::new(std::collections::HashMap::new()),
                    )
                    .await
                });

                // Spawn network handler if networking is enabled
                let (net_tx, net_handler) = if let Some(ref config) = net_config {
                    let (tx, rx) = mpsc::channel::<eryx::NetRequest>(32);
                    let manager = eryx::net::ConnectionManager::new(
                        config.clone(),
                        std::collections::HashMap::new(),
                    );
                    let task = tokio::spawn(async move {
                        eryx::callback_handler::run_net_handler(rx, manager).await;
                    });
                    (Some(tx), Some(task))
                } else {
                    (None, None)
                };

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

                // Execute with callbacks and optional output streaming / networking
                let mut builder = inner
                    .execute(&code)
                    .with_callbacks(&callbacks_vec, callback_tx)
                    .with_cancellation(token);

                if let Some(tx) = net_tx {
                    builder = builder.with_network(tx);
                }

                if let Some(tx) = output_tx {
                    builder = builder.with_output_streaming(tx);
                }

                let result = builder.run().await;

                // Wait for handler to finish (it will exit when channel closes)
                let _callback_count = handler.await.unwrap_or(0);

                // Wait for network handler to finish
                if let Some(handler) = net_handler {
                    let _ = handler.await;
                }

                // Wait for output collector to finish
                if let Some(collector) = output_collector {
                    let _ = collector.await;
                }

                result
            };

            Ok(match replay {
                None => Ok(body(inner).await),
                Some(state) => inner
                    .run_with_rollback(
                        &callbacks_vec,
                        || {
                            state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .suspended()
                                .is_some()
                        },
                        body,
                    )
                    .await
                    .map(|(result, _rolled_back)| result),
            })
        }
    }
}

/// A place in a session's call-order queue.
struct QueueSlot {
    /// Completes (with an error) when the call queued before this one ends.
    previous: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Dropped when this call ends, releasing the next one.
    done: tokio::sync::oneshot::Sender<()>,
}

impl QueueSlot {
    /// Wait for the previous call, returning the guard to hold while this
    /// call runs.
    async fn wait(self) -> tokio::sync::oneshot::Sender<()> {
        if let Some(previous) = self.previous {
            let _ = previous.await;
        }
        self.done
    }
}

/// What [`Session::run`] produces: the outer `Err` is a failed rollback, the
/// inner `Result` the execution outcome.
type RunOutcome = Result<Result<eryx::ExecutionOutput, eryx::Error>, eryx::Error>;

/// Turn a non-journaled [`Session::run`] outcome into `execute()`'s result.
fn flatten_run(outcome: RunOutcome) -> PyResult<ExecuteResult> {
    outcome
        .and_then(std::convert::identity)
        .map(ExecuteResult::from_execution_output)
        .map_err(eryx_error_to_py)
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let execution_count = self
            .inner
            .try_lock()
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
