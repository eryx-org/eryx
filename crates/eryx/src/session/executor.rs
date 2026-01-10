//! Session executor: manages persistent WASM instances for session reuse.
//!
//! This module provides the core infrastructure for keeping a WASM instance
//! alive between multiple `execute()` calls, avoiding the ~1.5ms Python
//! interpreter initialization overhead on each call.
//!
//! ## Architecture
//!
//! The `SessionExecutor` wraps a `Store<ExecutorState>` and the instantiated
//! component, keeping them alive between executions. This is in contrast to
//! the regular `PythonExecutor` which creates a fresh Store for each execution.
//!
//! ```text
//! Regular PythonExecutor:
//!   execute() -> new Store -> new Instance -> run -> drop Store
//!   execute() -> new Store -> new Instance -> run -> drop Store
//!   (Each call pays ~1.5ms Python init overhead)
//!
//! SessionExecutor:
//!   new() -> create Store -> create Instance
//!   execute() -> reuse Store/Instance -> run
//!   execute() -> reuse Store/Instance -> run
//!   drop() -> drop Store
//!   (Only first call pays Python init overhead)
//! ```
//!
//! ## State Persistence
//!
//! The Python runtime maintains persistent state between `execute()` calls.
//! Variables, functions, and classes defined in one call are available in
//! subsequent calls.
//!
//! State can be serialized via `snapshot_state()` and restored via
//! `restore_state()`, enabling state transfer between processes or
//! persistence to storage. The Python runtime uses pickle for serialization.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use wasmtime::Store;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder};

use crate::callback::Callback;
use crate::error::Error;
use crate::wasm::{
    CallbackRequest, ExecutionOutput, ExecutorState, HostCallbackInfo, MemoryTracker,
    PythonExecutor, Sandbox as SandboxBindings, TraceRequest,
};

/// Maximum snapshot size in bytes (10 MB).
///
/// This limit prevents abuse and ensures snapshots remain manageable.
/// The Python runtime enforces the same limit.
pub const MAX_SNAPSHOT_SIZE: usize = 10 * 1024 * 1024;

/// A snapshot of Python session state.
///
/// This captures all user-defined variables from the Python session,
/// serialized using pickle. The snapshot can be:
///
/// - Persisted to disk or a database
/// - Sent over the network to another process
/// - Restored later to continue execution with the same variables
///
/// # Example
///
/// ```rust,ignore
/// // Capture state after some executions
/// session.execute("x = 1").run().await?;
/// session.execute("y = 2").run().await?;
/// let snapshot = session.snapshot_state().await?;
///
/// // Save to bytes for storage
/// let bytes = snapshot.to_bytes();
///
/// // Later, restore in a new session
/// let snapshot = PythonStateSnapshot::from_bytes(&bytes)?;
/// new_session.restore_state(&snapshot).await?;
/// new_session.execute("print(x + y)").run().await?; // prints "3"
/// ```
#[derive(Debug, Clone)]
pub struct PythonStateSnapshot {
    /// Pickled Python state bytes.
    data: Vec<u8>,

    /// Metadata about when the snapshot was captured.
    metadata: SnapshotMetadata,
}

/// Metadata about a state snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotMetadata {
    /// Unix timestamp (milliseconds) when the snapshot was captured.
    pub timestamp_ms: u64,

    /// Size of the snapshot data in bytes.
    pub size_bytes: usize,
}

impl PythonStateSnapshot {
    /// Create a new snapshot from raw pickle data.
    fn new(data: Vec<u8>) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let size_bytes = data.len();

        Self {
            data,
            metadata: SnapshotMetadata {
                timestamp_ms,
                size_bytes,
            },
        }
    }

    /// Get the raw pickle data.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get snapshot metadata.
    #[must_use]
    pub fn metadata(&self) -> &SnapshotMetadata {
        &self.metadata
    }

    /// Get the size of the snapshot in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Convert the snapshot to bytes for storage or transmission.
    ///
    /// The format is simple: 8 bytes for timestamp, followed by pickle data.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.data.len());
        bytes.extend_from_slice(&self.metadata.timestamp_ms.to_le_bytes());
        bytes.extend_from_slice(&self.data);
        bytes
    }

    /// Restore a snapshot from bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is too short or corrupted.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 8 {
            return Err(Error::Snapshot("Snapshot data too short".to_string()));
        }

        let timestamp_ms = u64::from_le_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| Error::Snapshot("Invalid timestamp".to_string()))?,
        );

        let data = bytes[8..].to_vec();
        let size_bytes = data.len();

        Ok(Self {
            data,
            metadata: SnapshotMetadata {
                timestamp_ms,
                size_bytes,
            },
        })
    }
}

/// Builder for configuring and executing Python code in a session.
///
/// Created by [`SessionExecutor::execute`]. Use the builder methods to
/// configure callbacks and tracing, then call [`run`](Self::run) to execute.
///
/// # Example
///
/// ```rust,ignore
/// let output = session
///     .execute("print('hello')")
///     .with_callbacks(&callbacks, callback_tx)
///     .run()
///     .await?;
/// ```
pub struct SessionExecuteBuilder<'a> {
    session: &'a mut SessionExecutor,
    code: String,
    callbacks: Vec<Arc<dyn Callback>>,
    callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
}

impl std::fmt::Debug for SessionExecuteBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecuteBuilder")
            .field("code_len", &self.code.len())
            .field("callbacks_count", &self.callbacks.len())
            .field("has_callback_tx", &self.callback_tx.is_some())
            .field("has_trace_tx", &self.trace_tx.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> SessionExecuteBuilder<'a> {
    /// Create a new session execute builder.
    fn new(session: &'a mut SessionExecutor, code: impl Into<String>) -> Self {
        Self {
            session,
            code: code.into(),
            callbacks: Vec::new(),
            callback_tx: None,
            trace_tx: None,
        }
    }

    /// Set callbacks that Python code can invoke.
    ///
    /// The `callback_tx` channel is used to send callback requests from
    /// the WASM guest to the host for processing. Both the callbacks and
    /// the channel are required together since they work in tandem.
    #[must_use]
    pub fn with_callbacks(
        mut self,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: mpsc::Sender<CallbackRequest>,
    ) -> Self {
        self.callbacks = callbacks.to_vec();
        self.callback_tx = Some(callback_tx);
        self
    }

    /// Set the trace channel for receiving execution trace events.
    #[must_use]
    pub fn with_tracing(mut self, trace_tx: mpsc::UnboundedSender<TraceRequest>) -> Self {
        self.trace_tx = Some(trace_tx);
        self
    }

    /// Execute the Python code with the configured options.
    ///
    /// # Errors
    ///
    /// Returns an error if execution fails, times out, or the session is in use.
    pub async fn run(self) -> Result<ExecutionOutput, String> {
        self.session
            .execute_internal(&self.code, &self.callbacks, self.callback_tx, self.trace_tx)
            .await
    }
}

/// A session-aware executor that keeps WASM instances alive between executions.
///
/// Unlike `PythonExecutor` which creates a fresh instance for each execution,
/// `SessionExecutor` maintains state between calls, enabling:
///
/// - Faster subsequent executions (no Python init overhead)
/// - Persistent Python variables between calls
/// - REPL-style interactive execution
/// - State snapshots for persistence and transfer
pub struct SessionExecutor {
    /// The parent executor (for engine and instance_pre access).
    executor: Arc<PythonExecutor>,

    /// The store containing the WASM instance state.
    /// This is `Option` so we can take ownership during async execution.
    store: Option<Store<ExecutorState>>,

    /// The instantiated component bindings.
    /// This is `Option` so we can take ownership during async execution.
    bindings: Option<SandboxBindings>,

    /// Number of executions performed in this session.
    execution_count: u32,

    /// Optional execution timeout for epoch-based interruption.
    execution_timeout: Option<Duration>,
}

impl std::fmt::Debug for SessionExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecutor")
            .field("execution_count", &self.execution_count)
            .field("has_store", &self.store.is_some())
            .field("has_bindings", &self.bindings.is_some())
            .field("execution_timeout", &self.execution_timeout)
            .finish_non_exhaustive()
    }
}

impl SessionExecutor {
    /// Create a new session executor from a `PythonExecutor`.
    ///
    /// This instantiates the WASM component and keeps it alive for reuse.
    ///
    /// # Arguments
    ///
    /// * `executor` - The parent executor providing engine and instance_pre
    /// * `callbacks` - Callbacks available for this session
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be instantiated.
    pub async fn new(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
    ) -> Result<Self, Error> {
        // Build callback info for introspection
        let callback_infos: Vec<HostCallbackInfo> = callbacks
            .iter()
            .map(|cb| HostCallbackInfo {
                name: cb.name().to_string(),
                description: cb.description().to_string(),
                parameters_schema_json: serde_json::to_string(&cb.parameters_schema())
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();

        // Create WASI context with Python stdlib mounts if configured
        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.inherit_stdout().inherit_stderr();

        // Build PYTHONPATH from stdlib and all site-packages directories
        let site_packages_paths = executor.python_site_packages_paths();
        let mut pythonpath_parts = Vec::new();
        if executor.python_stdlib_path().is_some() {
            pythonpath_parts.push("/python-stdlib".to_string());
        }
        for i in 0..site_packages_paths.len() {
            pythonpath_parts.push(format!("/site-packages-{i}"));
        }

        // Mount Python stdlib if configured (required for eryx-wasm-runtime)
        if let Some(stdlib_path) = executor.python_stdlib_path() {
            wasi_builder.env("PYTHONHOME", "/python-stdlib");
            if !pythonpath_parts.is_empty() {
                wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
            }
            wasi_builder
                .preopened_dir(
                    stdlib_path,
                    "/python-stdlib",
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| Error::WasmEngine(format!("Failed to mount Python stdlib: {e}")))?;
        }

        // Mount each site-packages directory at a unique path
        for (i, site_packages_path) in site_packages_paths.iter().enumerate() {
            let mount_path = format!("/site-packages-{i}");
            wasi_builder
                .preopened_dir(
                    site_packages_path,
                    &mount_path,
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| Error::WasmEngine(format!("Failed to mount {mount_path}: {e}")))?;
        }

        let wasi = wasi_builder.build();

        let state = ExecutorState::new(
            wasi,
            ResourceTable::new(),
            None,
            None,
            callback_infos,
            MemoryTracker::new(None),
        );

        // Create store
        let mut store = Store::new(executor.engine(), state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set epoch deadline before instantiation - required when epoch_interruption is enabled
        // in the engine config. We use a very large value (but not u64::MAX to avoid overflow
        // when added to the current epoch).
        store.set_epoch_deadline(u64::MAX / 2);

        // Instantiate the component
        let bindings = executor
            .instance_pre()
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to instantiate component: {e}")))?;

        Ok(Self {
            executor,
            store: Some(store),
            bindings: Some(bindings),
            execution_count: 0,
            execution_timeout: None,
        })
    }

    /// Set the execution timeout for epoch-based interruption.
    ///
    /// When set, long-running Python code (including infinite loops like
    /// `while True: pass`) will be interrupted after the specified duration.
    /// This uses wasmtime's epoch-based interruption mechanism.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Optional timeout duration. Pass `None` to disable.
    pub fn set_execution_timeout(&mut self, timeout: Option<Duration>) {
        self.execution_timeout = timeout;
    }

    /// Get the current execution timeout.
    #[must_use]
    pub fn execution_timeout(&self) -> Option<Duration> {
        self.execution_timeout
    }

    /// Execute Python code with a fluent builder API.
    ///
    /// State from previous executions is preserved - variables defined in
    /// one call are accessible in subsequent calls.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Simple execution
    /// let output = session.execute("print('hello')").run().await?;
    ///
    /// // With callbacks
    /// let output = session
    ///     .execute("result = await my_callback()")
    ///     .with_callbacks(&callbacks, callback_tx)
    ///     .with_tracing(trace_tx)
    ///     .run()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn execute(&mut self, code: impl Into<String>) -> SessionExecuteBuilder<'_> {
        SessionExecuteBuilder::new(self, code)
    }

    /// Internal execute implementation with all parameters.
    ///
    /// This is called by [`SessionExecuteBuilder::run`].
    async fn execute_internal(
        &mut self,
        code: &str,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    ) -> Result<ExecutionOutput, String> {
        // Take ownership of store and bindings for async execution
        let mut store = self
            .store
            .take()
            .ok_or_else(|| "Store not available (concurrent execution?)".to_string())?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| "Bindings not available".to_string())?;

        // Update the executor state with new channels and callbacks
        let callback_infos: Vec<HostCallbackInfo> = callbacks
            .iter()
            .map(|cb| HostCallbackInfo {
                name: cb.name().to_string(),
                description: cb.description().to_string(),
                parameters_schema_json: serde_json::to_string(&cb.parameters_schema())
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();

        // Update state for this execution and reset memory tracker
        {
            let state = store.data_mut();
            state.set_callback_tx(callback_tx);
            state.set_trace_tx(trace_tx);
            state.set_callbacks(callback_infos);
            state.reset_memory_tracker();
        }

        self.execution_count += 1;

        tracing::debug!(
            code_len = code.len(),
            execution_count = self.execution_count,
            "SessionExecutor: executing Python code"
        );

        // Set up epoch-based deadline if timeout is configured.
        // This allows us to interrupt WASM execution even in tight loops.
        let execution_timeout = self.execution_timeout;
        let epoch_ticker = if let Some(timeout) = execution_timeout {
            // Set deadline to N epoch ticks from now (we'll increment epochs over time)
            // We use a granularity of 10ms for epoch ticks
            const EPOCH_TICK_MS: u64 = 10;
            let ticks_until_timeout = timeout.as_millis() as u64 / EPOCH_TICK_MS;
            // Ensure at least 1 tick
            let ticks = ticks_until_timeout.max(1);
            store.set_epoch_deadline(ticks);

            // Configure the store to trap when the epoch deadline is reached
            store.epoch_deadline_trap();

            // Spawn a thread to increment the engine's epoch periodically.
            // We use a std::thread instead of tokio::spawn because the WASM
            // execution may block the tokio runtime, preventing async tasks
            // from running.
            let engine = self.executor.engine().clone();
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_flag_clone = Arc::clone(&stop_flag);
            std::thread::spawn(move || {
                while !stop_flag_clone.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                    engine.increment_epoch();
                }
            });
            Some(stop_flag)
        } else {
            // No timeout - set a very high deadline that won't be reached
            // (but not u64::MAX to avoid overflow when added to current epoch)
            store.set_epoch_deadline(u64::MAX / 2);
            store.epoch_deadline_trap();
            None::<Arc<AtomicBool>>
        };

        // Execute the code
        let code_owned = code.to_string();
        let result = store
            .run_concurrent(async |accessor| bindings.call_execute(accessor, code_owned).await)
            .await;

        // Stop the epoch ticker thread if it was running
        if let Some(stop_flag) = epoch_ticker {
            stop_flag.store(true, Ordering::Relaxed);
        }

        // Clear channels after execution and capture peak memory
        let peak_memory = {
            let state = store.data_mut();
            state.set_callback_tx(None);
            state.set_trace_tx(None);
            state.peak_memory_bytes()
        };

        // Restore store and bindings before handling result
        self.store = Some(store);
        self.bindings = Some(bindings);

        // Process result - check for epoch deadline exceeded (timeout)
        let wasmtime_result = result.map_err(|e| {
            let err_str = format!("{e:?}");
            if err_str.contains("epoch deadline") || err_str.contains("wasm trap: interrupt") {
                format!(
                    "Execution timed out after {:?}",
                    execution_timeout.unwrap_or_default()
                )
            } else {
                format!("WASM execution error: {e:?}")
            }
        })?;
        let stdout = wasmtime_result.map_err(|e| format!("WASM execution error: {e:?}"))??;
        Ok(ExecutionOutput::new(stdout, peak_memory))
    }

    /// Get the number of executions performed in this session.
    #[must_use]
    pub fn execution_count(&self) -> u32 {
        self.execution_count
    }

    /// Reset the session to a fresh state.
    ///
    /// This creates a new WASM instance, discarding all previous state.
    ///
    /// # Arguments
    ///
    /// * `callbacks` - Callbacks for the new session
    ///
    /// # Errors
    ///
    /// Returns an error if re-instantiation fails.
    pub async fn reset(&mut self, callbacks: &[Arc<dyn Callback>]) -> Result<(), Error> {
        // Build callback info
        let callback_infos: Vec<HostCallbackInfo> = callbacks
            .iter()
            .map(|cb| HostCallbackInfo {
                name: cb.name().to_string(),
                description: cb.description().to_string(),
                parameters_schema_json: serde_json::to_string(&cb.parameters_schema())
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();

        // Create WASI context with Python stdlib mounts if configured (same as new())
        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.inherit_stdout().inherit_stderr();

        // Build PYTHONPATH from stdlib and all site-packages directories
        let site_packages_paths = self.executor.python_site_packages_paths();
        let mut pythonpath_parts = Vec::new();
        if self.executor.python_stdlib_path().is_some() {
            pythonpath_parts.push("/python-stdlib".to_string());
        }
        for i in 0..site_packages_paths.len() {
            pythonpath_parts.push(format!("/site-packages-{i}"));
        }

        // Mount Python stdlib if configured (required for eryx-wasm-runtime)
        if let Some(stdlib_path) = self.executor.python_stdlib_path() {
            wasi_builder.env("PYTHONHOME", "/python-stdlib");
            if !pythonpath_parts.is_empty() {
                wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
            }
            wasi_builder
                .preopened_dir(
                    stdlib_path,
                    "/python-stdlib",
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| Error::WasmEngine(format!("Failed to mount Python stdlib: {e}")))?;
        }

        // Mount each site-packages directory at a unique path
        for (i, site_packages_path) in site_packages_paths.iter().enumerate() {
            let mount_path = format!("/site-packages-{i}");
            wasi_builder
                .preopened_dir(
                    site_packages_path,
                    &mount_path,
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| Error::WasmEngine(format!("Failed to mount {mount_path}: {e}")))?;
        }

        let wasi = wasi_builder.build();

        let state = ExecutorState::new(
            wasi,
            ResourceTable::new(),
            None,
            None,
            callback_infos,
            MemoryTracker::new(None),
        );

        // Create new store
        let mut store = Store::new(self.executor.engine(), state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set epoch deadline before instantiation - required when epoch_interruption is enabled
        // in the engine config. We use a very large value (but not u64::MAX to avoid overflow
        // when added to the current epoch).
        store.set_epoch_deadline(u64::MAX / 2);

        // Preserve execution timeout setting across reset
        let execution_timeout = self.execution_timeout;

        // Instantiate the component
        let bindings = self
            .executor
            .instance_pre()
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to reinstantiate component: {e}")))?;

        self.store = Some(store);
        self.bindings = Some(bindings);
        self.execution_count = 0;
        self.execution_timeout = execution_timeout;

        Ok(())
    }

    /// Get a reference to the underlying store.
    ///
    /// This is primarily for debugging and introspection purposes.
    ///
    /// # Returns
    ///
    /// `None` if the store is currently in use by an async execution.
    #[must_use]
    pub fn store(&self) -> Option<&Store<ExecutorState>> {
        self.store.as_ref()
    }

    /// Get a mutable reference to the underlying store.
    ///
    /// # Safety
    ///
    /// Modifying the store directly may put the session in an inconsistent state.
    ///
    /// # Returns
    ///
    /// `None` if the store is currently in use by an async execution.
    #[must_use]
    pub fn store_mut(&mut self) -> Option<&mut Store<ExecutorState>> {
        self.store.as_mut()
    }

    // =========================================================================
    // State Snapshot Methods (WIT Export Approach)
    // =========================================================================

    /// Capture a snapshot of the current Python session state.
    ///
    /// This calls the Python runtime's `snapshot_state()` export, which uses
    /// pickle to serialize all user-defined variables, functions, and classes.
    ///
    /// # Returns
    ///
    /// A `PythonStateSnapshot` that can be serialized and restored later.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The WASM call fails
    /// - Serialization fails (e.g., unpicklable objects)
    /// - The snapshot exceeds the size limit
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").run().await?;
    /// let snapshot = session.snapshot_state().await?;
    /// println!("Snapshot size: {} bytes", snapshot.size());
    /// ```
    pub async fn snapshot_state(&mut self) -> Result<PythonStateSnapshot, Error> {
        // Take ownership of store and bindings
        let mut store = self
            .store
            .take()
            .ok_or_else(|| Error::WasmEngine("Store not available".to_string()))?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| Error::WasmEngine("Bindings not available".to_string()))?;

        tracing::debug!("SessionExecutor: capturing state snapshot");

        // Call the snapshot_state export using run_concurrent (async function)
        let result = store
            .run_concurrent(async |accessor| bindings.call_snapshot_state(accessor).await)
            .await;

        // Restore store and bindings
        self.store = Some(store);
        self.bindings = Some(bindings);

        // Process result
        let wasmtime_result =
            result.map_err(|e| Error::WasmEngine(format!("WASM snapshot error: {e}")))?;

        let inner_result =
            wasmtime_result.map_err(|e| Error::WasmEngine(format!("WASM snapshot error: {e}")))?;

        let data = inner_result.map_err(Error::Snapshot)?;

        // Check size limit
        if data.len() > MAX_SNAPSHOT_SIZE {
            return Err(Error::Snapshot(format!(
                "Snapshot too large: {} bytes (max {} bytes)",
                data.len(),
                MAX_SNAPSHOT_SIZE
            )));
        }

        tracing::debug!(size_bytes = data.len(), "State snapshot captured");

        Ok(PythonStateSnapshot::new(data))
    }

    /// Restore Python session state from a previously captured snapshot.
    ///
    /// After restore, subsequent `execute()` calls will have access to all
    /// variables that were present when the snapshot was taken.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The snapshot to restore
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The WASM call fails
    /// - Deserialization fails (e.g., corrupted data)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Restore from a previously saved snapshot
    /// let snapshot = PythonStateSnapshot::from_bytes(&saved_bytes)?;
    /// session.restore_state(&snapshot).await?;
    ///
    /// // Variables from the snapshot are now available
    /// session.execute("print(x)", &[], None, None).await?;
    /// ```
    pub async fn restore_state(&mut self, snapshot: &PythonStateSnapshot) -> Result<(), Error> {
        // Take ownership of store and bindings
        let mut store = self
            .store
            .take()
            .ok_or_else(|| Error::WasmEngine("Store not available".to_string()))?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| Error::WasmEngine("Bindings not available".to_string()))?;

        tracing::debug!(
            size_bytes = snapshot.size(),
            "SessionExecutor: restoring state snapshot"
        );

        // Call the restore_state export using run_concurrent (async function)
        let data = snapshot.data().to_vec();
        let result = store
            .run_concurrent(async |accessor| bindings.call_restore_state(accessor, data).await)
            .await;

        // Restore store and bindings
        self.store = Some(store);
        self.bindings = Some(bindings);

        // Process result
        let wasmtime_result =
            result.map_err(|e| Error::WasmEngine(format!("WASM restore error: {e}")))?;

        let inner_result =
            wasmtime_result.map_err(|e| Error::WasmEngine(format!("WASM restore error: {e}")))?;

        inner_result.map_err(Error::Snapshot)?;

        tracing::debug!("State snapshot restored");

        Ok(())
    }

    /// Clear all persistent state from the session.
    ///
    /// After clear, subsequent `execute()` calls will start with a fresh
    /// namespace (no user-defined variables from previous calls).
    ///
    /// This is lighter-weight than `reset()` because it doesn't recreate
    /// the WASM instance - it just clears the Python-level state.
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM call fails.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").run().await?;
    /// session.clear_state().await?;
    /// // x is no longer defined
    /// ```
    pub async fn clear_state(&mut self) -> Result<(), Error> {
        // Take ownership of store and bindings
        let mut store = self
            .store
            .take()
            .ok_or_else(|| Error::WasmEngine("Store not available".to_string()))?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| Error::WasmEngine("Bindings not available".to_string()))?;

        tracing::debug!("SessionExecutor: clearing state");

        // Call the clear_state export using run_concurrent (async function)
        let result = store
            .run_concurrent(async |accessor| bindings.call_clear_state(accessor).await)
            .await;

        // Restore store and bindings
        self.store = Some(store);
        self.bindings = Some(bindings);

        // Process result
        let wasmtime_result =
            result.map_err(|e| Error::WasmEngine(format!("WASM clear state error: {e}")))?;

        wasmtime_result.map_err(|e| Error::WasmEngine(format!("WASM clear state error: {e}")))?;

        tracing::debug!("State cleared");

        Ok(())
    }
}

// ============================================================================
// ExecutorState Extensions
// ============================================================================
//
// These methods extend ExecutorState to support session reuse by allowing
// channels and callbacks to be updated between executions.

impl ExecutorState {
    /// Create a new ExecutorState with the given configuration.
    pub(crate) fn new(
        wasi: WasiCtx,
        table: ResourceTable,
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
        callbacks: Vec<HostCallbackInfo>,
        memory_tracker: MemoryTracker,
    ) -> Self {
        Self {
            wasi,
            table,
            callback_tx,
            trace_tx,
            callbacks,
            memory_tracker,
        }
    }

    /// Update the callback channel for a new execution.
    pub(crate) fn set_callback_tx(&mut self, tx: Option<mpsc::Sender<CallbackRequest>>) {
        self.callback_tx = tx;
    }

    /// Update the trace channel for a new execution.
    pub(crate) fn set_trace_tx(&mut self, tx: Option<mpsc::UnboundedSender<TraceRequest>>) {
        self.trace_tx = tx;
    }

    /// Update the available callbacks for a new execution.
    pub(crate) fn set_callbacks(&mut self, callbacks: Vec<HostCallbackInfo>) {
        self.callbacks = callbacks;
    }

    /// Get the peak memory usage from the tracker.
    pub(crate) fn peak_memory_bytes(&self) -> u64 {
        self.memory_tracker.peak_memory_bytes()
    }

    /// Reset the memory tracker for a new execution.
    pub(crate) fn reset_memory_tracker(&self) {
        self.memory_tracker.reset();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_session_executor_debug() {
        // Just verify the Debug impl compiles
        let _fmt = format!("{:?}", "SessionExecutor placeholder");
    }

    #[test]
    fn test_python_state_snapshot_roundtrip() {
        let data = vec![1, 2, 3, 4, 5];
        let snapshot = PythonStateSnapshot::new(data.clone());

        assert_eq!(snapshot.data(), &data);
        assert_eq!(snapshot.size(), 5);
        assert!(snapshot.metadata().timestamp_ms > 0);

        // Test serialization roundtrip
        let bytes = snapshot.to_bytes();
        let restored = PythonStateSnapshot::from_bytes(&bytes).expect("from_bytes failed");

        assert_eq!(restored.data(), &data);
        assert_eq!(
            restored.metadata().timestamp_ms,
            snapshot.metadata().timestamp_ms
        );
    }

    #[test]
    fn test_python_state_snapshot_from_bytes_too_short() {
        let bytes = vec![1, 2, 3]; // Less than 8 bytes
        let result = PythonStateSnapshot::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_snapshot_metadata() {
        let snapshot = PythonStateSnapshot::new(vec![0; 100]);
        let meta = snapshot.metadata();

        assert_eq!(meta.size_bytes, 100);
        assert!(meta.timestamp_ms > 0);
    }
}
