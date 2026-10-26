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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasmtime::Store;
use wasmtime::component::{Accessor, ResourceTable};
// wasmtime 48 collapsed DirPerms/FilePerms into FsPerms; the fine-grained
// flags the VFS enforces now live in eryx-vfs. Only `build_hybrid_vfs_context`
// uses them, so the import carries that function's feature gate.
#[cfg(feature = "vfs")]
use eryx_vfs::{DirPerms, FilePerms};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder};

use crate::callback::Callback;
use crate::error::Error;
use crate::sandbox::ResourceLimits;
use crate::wasm::{
    CallbackRequest, ExecutionOutput, ExecutorState, FdOutput, HostCallbackInfo, MemoryTracker,
    NetRequest, PythonExecutor, Sandbox as SandboxBindings, StreamBudget, TraceRequest,
};

/// Maximum snapshot size in bytes (10 MB).
///
/// This limit prevents abuse and ensures snapshots remain manageable.
/// The Python runtime enforces the same limit.
pub const MAX_SNAPSHOT_SIZE: usize = 10 * 1024 * 1024;

/// Time bound for snapshot, restore and clear when the session has no
/// execution timeout.
pub const DEFAULT_STATE_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Smallest time bound for snapshot, restore and clear. The first snapshot
/// loads the serializer, which can take longer than a tight execution timeout.
pub const MIN_STATE_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Smallest fuel bound for snapshot, restore and clear, about ten times what
/// loading the serializer costs.
pub const MIN_STATE_OP_FUEL: u64 = 20_000_000_000;

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
#[non_exhaustive]
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
    preamble: String,
    callbacks: Vec<Arc<dyn Callback>>,
    callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    net_tx: Option<mpsc::Sender<NetRequest>>,
    output_tx: Option<mpsc::UnboundedSender<crate::wasm::OutputRequest>>,
    fuel_limit: Option<u64>,
    cancellation_token: Option<CancellationToken>,
}

impl std::fmt::Debug for SessionExecuteBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecuteBuilder")
            .field("code_len", &self.code.len())
            .field("callbacks_count", &self.callbacks.len())
            .field("has_callback_tx", &self.callback_tx.is_some())
            .field("has_trace_tx", &self.trace_tx.is_some())
            .field("has_net_tx", &self.net_tx.is_some())
            .field("has_output_tx", &self.output_tx.is_some())
            .field("fuel_limit", &self.fuel_limit)
            .field("has_cancellation_token", &self.cancellation_token.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> SessionExecuteBuilder<'a> {
    /// Create a new session execute builder.
    fn new(session: &'a mut SessionExecutor, code: impl Into<String>) -> Self {
        Self {
            session,
            code: code.into(),
            preamble: String::new(),
            callbacks: Vec::new(),
            callback_tx: None,
            trace_tx: None,
            net_tx: None,
            output_tx: None,
            fuel_limit: None,
            cancellation_token: None,
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

    /// Enable networking for this execution.
    ///
    /// The `net_tx` channel is used to send network requests (TCP/TLS) from
    /// the WASM guest to the host for processing.
    #[must_use]
    pub fn with_network(mut self, net_tx: mpsc::Sender<NetRequest>) -> Self {
        self.net_tx = Some(net_tx);
        self
    }

    /// Enable real-time output streaming for this execution.
    #[must_use]
    pub fn with_output_streaming(
        mut self,
        output_tx: mpsc::UnboundedSender<crate::wasm::OutputRequest>,
    ) -> Self {
        self.output_tx = Some(output_tx);
        self
    }

    /// Run `preamble` (setup such as libraries or secrets) before the code, in
    /// the same namespace.
    ///
    /// The preamble is compiled separately, so tracebacks and trace events keep
    /// the code's own line numbers, and the preamble's lines are not traced.
    #[must_use]
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = preamble.into();
        self
    }

    /// Set the maximum fuel (instructions) allowed for this execution.
    ///
    /// Fuel provides fine-grained, deterministic execution bounds at the
    /// instruction level. When fuel runs out, execution traps.
    ///
    /// This overrides the session's default fuel limit for this execution only.
    /// If neither this nor the session's fuel limit is set, fuel defaults to
    /// `u64::MAX` for tracking-only mode (fuel consumed is still reported).
    #[must_use]
    pub fn with_fuel_limit(mut self, fuel: u64) -> Self {
        self.fuel_limit = Some(fuel);
        self
    }

    /// Set a cancellation token for external cancellation support.
    ///
    /// When the token is cancelled, the execution is interrupted and returns
    /// [`Error::Cancelled`]. The instance is discarded, so the next call on
    /// the session resets it first: Python state from before the cancelled
    /// call is lost.
    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Execute the Python code with the configured options.
    ///
    /// # Errors
    ///
    /// Returns an error if execution fails, times out, exceeds fuel limit, or the session is in use.
    #[tracing::instrument(
        name = "SessionExecuteBuilder::run",
        skip(self),
        fields(
            code_len = self.code.len(),
            callbacks = self.callbacks.len(),
            fuel_limit = ?self.fuel_limit,
        )
    )]
    pub async fn run(self) -> Result<ExecutionOutput, Error> {
        self.session
            .execute_internal(
                &crate::wasm::frame_code(&self.preamble, &self.code),
                &self.callbacks,
                self.callback_tx,
                self.trace_tx,
                self.net_tx,
                self.output_tx,
                self.fuel_limit,
                self.cancellation_token,
            )
            .await
    }
}

/// A host filesystem volume mount.
///
/// Maps a host directory into the sandbox at a specified guest path,
/// using cap-std for capability-based security.
#[cfg(feature = "vfs")]
#[derive(Debug, Clone)]
pub struct VolumeMount {
    /// Absolute path on the host filesystem.
    pub host_path: std::path::PathBuf,
    /// Path visible inside the sandbox (e.g., "/mnt/data").
    pub guest_path: String,
    /// If true, the mount is read-only.
    pub read_only: bool,
}

#[cfg(feature = "vfs")]
impl VolumeMount {
    /// Create a new read-write volume mount.
    #[must_use]
    pub fn new(host_path: impl Into<std::path::PathBuf>, guest_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            guest_path: guest_path.into(),
            read_only: false,
        }
    }

    /// Create a new read-only volume mount.
    #[must_use]
    pub fn read_only(
        host_path: impl Into<std::path::PathBuf>,
        guest_path: impl Into<String>,
    ) -> Self {
        Self {
            host_path: host_path.into(),
            guest_path: guest_path.into(),
            read_only: true,
        }
    }
}

/// Configuration for the virtual filesystem (VFS) in a session.
///
/// This allows customizing the VFS mount path and permissions.
#[cfg(feature = "vfs")]
#[derive(Debug, Clone)]
pub struct VfsConfig {
    /// The guest path where VFS storage is mounted (default: "/data").
    pub mount_path: String,
    /// Directory permissions for the VFS mount.
    pub dir_perms: eryx_vfs::DirPerms,
    /// File permissions for files in the VFS mount.
    pub file_perms: eryx_vfs::FilePerms,
    /// Host filesystem volume mounts.
    pub volumes: Vec<VolumeMount>,
}

#[cfg(feature = "vfs")]
impl Default for VfsConfig {
    fn default() -> Self {
        Self {
            mount_path: "/data".to_string(),
            dir_perms: eryx_vfs::DirPerms::all(),
            file_perms: eryx_vfs::FilePerms::all(),
            volumes: Vec::new(),
        }
    }
}

#[cfg(feature = "vfs")]
impl VfsConfig {
    /// Create a new VFS config with the given mount path.
    ///
    /// Uses full read/write permissions by default.
    #[must_use]
    pub fn new(mount_path: impl Into<String>) -> Self {
        Self {
            mount_path: mount_path.into(),
            ..Default::default()
        }
    }

    /// Set directory permissions.
    #[must_use]
    pub fn with_dir_perms(mut self, perms: eryx_vfs::DirPerms) -> Self {
        self.dir_perms = perms;
        self
    }

    /// Set file permissions.
    #[must_use]
    pub fn with_file_perms(mut self, perms: eryx_vfs::FilePerms) -> Self {
        self.file_perms = perms;
        self
    }

    /// Add a host filesystem volume mount.
    #[must_use]
    pub fn with_volume(mut self, volume: VolumeMount) -> Self {
        self.volumes.push(volume);
        self
    }

    /// Add multiple host filesystem volume mounts.
    #[must_use]
    pub fn with_volumes(mut self, volumes: impl IntoIterator<Item = VolumeMount>) -> Self {
        self.volumes.extend(volumes);
        self
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

    /// Whether the Python state was lost (and the instance reset) since the
    /// last successful execution; reported once via
    /// [`ExecutionOutput::state_was_reset`].
    state_was_reset: bool,

    /// Optional execution timeout for epoch-based interruption.
    execution_timeout: Option<Duration>,

    /// Optional fuel limit for instruction tracking/limiting.
    fuel_limit: Option<u64>,

    /// Time bound for snapshot/restore/clear, overriding the one derived
    /// from `execution_timeout`.
    state_op_timeout: Option<Duration>,

    /// Cancels snapshot/restore/clear; see
    /// [`set_state_op_cancellation`](SessionExecutor::set_state_op_cancellation).
    state_op_cancellation: Option<CancellationToken>,

    /// Maximum guest memory, preserved when the store is recreated.
    memory_limit: Option<u64>,

    /// The guest's direct fd 1/2 writes, appended to each execution's output.
    fd_output: FdOutput,

    /// Peak memory and fuel consumed by the most recent execution, kept so
    /// callers can report them when the execution failed.
    last_peak_memory_bytes: u64,
    last_fuel_consumed: Option<u64>,

    /// Whether the most recent execution dropped streamed output (not trace
    /// events) for exceeding the stream budget.
    last_output_truncated: bool,

    /// VFS storage that persists across resets.
    #[cfg(feature = "vfs")]
    vfs_storage: Option<eryx_vfs::ArcStorage>,

    /// VFS configuration that persists across resets.
    #[cfg(feature = "vfs")]
    vfs_config: Option<VfsConfig>,

    /// Quota used when creating the session's own VFS storage.
    #[cfg(feature = "vfs")]
    vfs_max_bytes: Option<u64>,
}

impl std::fmt::Debug for SessionExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecutor")
            .field("execution_count", &self.execution_count)
            .field("has_store", &self.store.is_some())
            .field("has_bindings", &self.bindings.is_some())
            .field("execution_timeout", &self.execution_timeout)
            .field("fuel_limit", &self.fuel_limit)
            .field("memory_limit", &self.memory_limit)
            .finish_non_exhaustive()
    }
}

/// Build callback info for introspection from a slice of callbacks.
fn build_callback_infos(callbacks: &[Arc<dyn Callback>]) -> Vec<HostCallbackInfo> {
    callbacks
        .iter()
        .map(|cb| HostCallbackInfo {
            name: cb.name().to_string(),
            description: cb.description().to_string(),
            parameters_schema_json: serde_json::to_string(&cb.parameters_schema())
                .unwrap_or_else(|_| "{}".to_string()),
        })
        .collect()
}

/// Build WASI context with Python stdlib and site-packages mounts.
fn build_wasi_context(executor: &PythonExecutor, fd_output: &FdOutput) -> Result<WasiCtx, Error> {
    let mut wasi_builder = WasiCtxBuilder::new();
    fd_output.attach(&mut wasi_builder);

    // Build PYTHONPATH from stdlib and all site-packages directories
    let site_packages_paths = executor.python_site_packages_paths();
    let mut pythonpath_parts = Vec::new();
    if executor.python_stdlib_path().is_some() {
        pythonpath_parts.push("/python-stdlib".to_string());
    }
    for i in 0..site_packages_paths.len() {
        pythonpath_parts.push(crate::wasm::site_packages_mount_path(i));
    }

    // Mount Python stdlib if configured (required for eryx-wasm-runtime)
    if let Some(stdlib_path) = executor.python_stdlib_path() {
        wasi_builder.env("PYTHONHOME", "/python-stdlib");
        if !pythonpath_parts.is_empty() {
            wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
        }
        wasi_builder
            .preopened_dir(stdlib_path, "/python-stdlib", FsPerms::ReadOnly)
            .map_err(|e| Error::WasmEngine(format!("Failed to mount Python stdlib: {e}")))?;
    }

    // Mount each site-packages directory (index 0 keeps the baked sys.path)
    for (i, site_packages_path) in site_packages_paths.iter().enumerate() {
        let mount_path = crate::wasm::site_packages_mount_path(i);
        wasi_builder
            .preopened_dir(site_packages_path, &mount_path, FsPerms::ReadOnly)
            .map_err(|e| Error::WasmEngine(format!("Failed to mount {mount_path}: {e}")))?;
    }

    Ok(wasi_builder.build())
}

/// Build hybrid VFS context with VFS storage and real filesystem preopens.
#[cfg(feature = "vfs")]
fn build_hybrid_vfs_context(
    executor: &PythonExecutor,
    vfs_storage: eryx_vfs::ArcStorage,
    vfs_config: &VfsConfig,
) -> std::result::Result<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>, Error> {
    let mut ctx = eryx_vfs::HybridVfsCtx::new(vfs_storage);

    // Add a writable VFS directory backed by VFS storage
    ctx.add_vfs_preopen(
        &vfs_config.mount_path,
        vfs_config.dir_perms,
        vfs_config.file_perms,
    );

    // Add real filesystem preopens that mirror the WASI preopens
    if let Some(stdlib_path) = executor.python_stdlib_path()
        && let Ok(real_dir) =
            eryx_vfs::RealDir::open_ambient(stdlib_path, DirPerms::READ, FilePerms::READ)
    {
        ctx.add_real_preopen("/python-stdlib", real_dir);
    }
    for (i, site_packages_path) in executor.python_site_packages_paths().iter().enumerate() {
        let mount_path = crate::wasm::site_packages_mount_path(i);
        if let Ok(real_dir) =
            eryx_vfs::RealDir::open_ambient(site_packages_path, DirPerms::READ, FilePerms::READ)
        {
            ctx.add_real_preopen(&mount_path, real_dir);
        }
    }

    // Add user-specified host filesystem volume mounts
    for volume in &vfs_config.volumes {
        let (dir_perms, file_perms) = if volume.read_only {
            (DirPerms::READ, FilePerms::READ)
        } else {
            (DirPerms::all(), FilePerms::all())
        };
        let result = if volume.host_path.is_file() {
            ctx.add_real_file_preopen_path(
                &volume.guest_path,
                &volume.host_path,
                dir_perms,
                file_perms,
            )
        } else {
            ctx.add_real_preopen_path(&volume.guest_path, &volume.host_path, dir_perms, file_perms)
        };
        result.map_err(|e| {
            Error::WasmEngine(format!(
                "Failed to mount volume {} -> {}: {e}",
                volume.host_path.display(),
                volume.guest_path,
            ))
        })?;
    }

    Ok(ctx)
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
    #[tracing::instrument(
        name = "SessionExecutor::new",
        skip(executor, callbacks),
        fields(callbacks = callbacks.len())
    )]
    pub async fn new(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
    ) -> Result<Self, Error> {
        Self::new_with_limits(executor, callbacks, &ResourceLimits::unlimited()).await
    }

    /// Create a session with the supplied execution and resource limits.
    ///
    /// The limits are applied before component instantiation, so the memory
    /// cap also bounds initial memory growth. The execution timeout and fuel
    /// limit apply to every execution and are retained when the session is
    /// reset. Callback timeout and invocation-count limits remain the
    /// responsibility of the callback handler; this low-level executor only
    /// forwards callback requests.
    ///
    /// # Errors
    ///
    /// Returns an error if the component cannot be instantiated.
    pub async fn new_with_limits(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        limits: &ResourceLimits,
    ) -> Result<Self, Error> {
        #[cfg(feature = "vfs")]
        {
            Self::new_internal(executor, callbacks, None, None, limits).await
        }
        #[cfg(not(feature = "vfs"))]
        {
            Self::new_internal(executor, callbacks, limits).await
        }
    }

    /// Create a new session executor with a custom VFS storage.
    ///
    /// This allows providing an external `InMemoryStorage` that persists
    /// across session resets. Files written to the VFS mount path (default: `/data/*`)
    /// will be stored in the provided storage.
    ///
    /// # Arguments
    ///
    /// * `executor` - The parent executor providing engine and instance_pre
    /// * `callbacks` - Callbacks available for this session
    /// * `vfs_storage` - The VFS storage to use for the VFS mount
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be instantiated.
    #[cfg(feature = "vfs")]
    pub async fn new_with_vfs(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        vfs_storage: eryx_vfs::ArcStorage,
    ) -> Result<Self, Error> {
        Self::new_internal(
            executor,
            callbacks,
            Some(vfs_storage),
            None,
            &ResourceLimits::unlimited(),
        )
        .await
    }

    /// Create a new session executor with custom VFS storage and configuration.
    ///
    /// This allows full control over VFS mount path and permissions.
    ///
    /// # Arguments
    ///
    /// * `executor` - The parent executor providing engine and instance_pre
    /// * `callbacks` - Callbacks available for this session
    /// * `vfs_storage` - The VFS storage to use
    /// * `vfs_config` - Configuration for the VFS mount (path, permissions)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::{PythonExecutor, SessionExecutor, VfsConfig};
    /// use eryx::vfs::InMemoryStorage;
    /// use std::sync::Arc;
    ///
    /// let storage = Arc::new(InMemoryStorage::new());
    /// let config = VfsConfig::new("/workspace");  // Custom mount path
    /// let session = SessionExecutor::new_with_vfs_config(
    ///     executor,
    ///     &[],
    ///     storage,
    ///     config,
    /// ).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be instantiated.
    #[cfg(feature = "vfs")]
    pub async fn new_with_vfs_config(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        vfs_storage: eryx_vfs::ArcStorage,
        vfs_config: VfsConfig,
    ) -> Result<Self, Error> {
        Self::new_internal(
            executor,
            callbacks,
            Some(vfs_storage),
            Some(vfs_config),
            &ResourceLimits::unlimited(),
        )
        .await
    }

    /// Create a session with caller-owned VFS storage and resource limits.
    ///
    /// The supplied storage retains ownership of its own quota and policy;
    /// `limits.max_vfs_bytes` is only used for storage created by Eryx.
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be instantiated or a VFS
    /// mount cannot be created.
    #[cfg(feature = "vfs")]
    pub async fn new_with_vfs_config_and_limits(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        vfs_storage: eryx_vfs::ArcStorage,
        vfs_config: VfsConfig,
        limits: &ResourceLimits,
    ) -> Result<Self, Error> {
        Self::new_internal(
            executor,
            callbacks,
            Some(vfs_storage),
            Some(vfs_config),
            limits,
        )
        .await
    }

    /// Internal constructor with optional VFS storage and config.
    #[cfg(feature = "vfs")]
    async fn new_internal(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        vfs_storage: Option<eryx_vfs::ArcStorage>,
        vfs_config: Option<VfsConfig>,
        limits: &ResourceLimits,
    ) -> Result<Self, Error> {
        let callback_infos = build_callback_infos(callbacks);
        let fd_output = FdOutput::default();
        let wasi = build_wasi_context(&executor, &fd_output)?;

        // Use provided storage/config or create defaults. A storage supplied by
        // the caller owns its own quota; max_vfs_bytes applies only here.
        let vfs_storage = vfs_storage.unwrap_or_else(|| {
            eryx_vfs::ArcStorage::new(std::sync::Arc::new(
                eryx_vfs::InMemoryStorage::with_max_bytes(
                    limits.max_vfs_bytes.unwrap_or(eryx_vfs::DEFAULT_MAX_BYTES),
                ),
            ))
        });
        let vfs_config = vfs_config.unwrap_or_default();

        let hybrid_vfs_ctx = Some(build_hybrid_vfs_context(
            &executor,
            vfs_storage.clone(),
            &vfs_config,
        )?);

        let state = ExecutorState::new(
            wasi,
            ResourceTable::new(),
            None,
            None,
            callback_infos,
            MemoryTracker::new(limits.max_memory_bytes),
            hybrid_vfs_ctx,
        );

        // Create store
        let mut store = Store::new(executor.engine(), state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set epoch deadline before instantiation - required when epoch_interruption is enabled
        // in the engine config. We use a very large value (but not u64::MAX to avoid overflow
        // when added to the current epoch).
        store.set_epoch_deadline(u64::MAX / 2);

        // Set initial fuel - required when consume_fuel is enabled in the engine config.
        // We use u64::MAX for tracking-only mode; actual limits are applied per-execution.
        store
            .set_fuel(u64::MAX)
            .map_err(|e| Error::WasmEngine(format!("Failed to set fuel: {e}")))?;

        // Instantiate the component
        let bindings = executor
            .instance_pre()
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to instantiate component: {e}")))?;

        // Configure the result-capture variable name once for the session
        // (the guest defaults to "result", so only call for a custom name).
        if executor.result_variable() != "result" {
            let name = executor.result_variable().to_string();
            store
                .run_concurrent(async |accessor| {
                    bindings.call_set_result_variable(accessor, name).await
                })
                .await
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?;
        }

        Ok(Self {
            executor,
            store: Some(store),
            bindings: Some(bindings),
            execution_count: 0,
            state_was_reset: false,
            execution_timeout: limits.execution_timeout,
            fuel_limit: limits.max_fuel,
            state_op_timeout: None,
            state_op_cancellation: None,
            memory_limit: limits.max_memory_bytes,
            fd_output,
            last_peak_memory_bytes: 0,
            last_fuel_consumed: None,
            last_output_truncated: false,
            vfs_storage: Some(vfs_storage),
            vfs_config: Some(vfs_config),
            vfs_max_bytes: limits.max_vfs_bytes,
        })
    }

    /// Internal constructor without VFS.
    #[cfg(not(feature = "vfs"))]
    async fn new_internal(
        executor: Arc<PythonExecutor>,
        callbacks: &[Arc<dyn Callback>],
        limits: &ResourceLimits,
    ) -> Result<Self, Error> {
        let callback_infos = build_callback_infos(callbacks);
        let fd_output = FdOutput::default();
        let wasi = build_wasi_context(&executor, &fd_output)?;

        let state = ExecutorState::new(
            wasi,
            ResourceTable::new(),
            None,
            None,
            callback_infos,
            MemoryTracker::new(limits.max_memory_bytes),
        );

        // Create store
        let mut store = Store::new(executor.engine(), state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set epoch deadline before instantiation
        store.set_epoch_deadline(u64::MAX / 2);

        // Set initial fuel - required when consume_fuel is enabled in the engine config.
        // We use u64::MAX for tracking-only mode; actual limits are applied per-execution.
        store
            .set_fuel(u64::MAX)
            .map_err(|e| Error::WasmEngine(format!("Failed to set fuel: {e}")))?;

        // Instantiate the component
        let bindings = executor
            .instance_pre()
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to instantiate component: {e}")))?;

        // Configure the result-capture variable name once for the session
        // (the guest defaults to "result", so only call for a custom name).
        if executor.result_variable() != "result" {
            let name = executor.result_variable().to_string();
            store
                .run_concurrent(async |accessor| {
                    bindings.call_set_result_variable(accessor, name).await
                })
                .await
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?;
        }

        Ok(Self {
            executor,
            store: Some(store),
            bindings: Some(bindings),
            execution_count: 0,
            state_was_reset: false,
            execution_timeout: limits.execution_timeout,
            fuel_limit: limits.max_fuel,
            state_op_timeout: None,
            state_op_cancellation: None,
            memory_limit: limits.max_memory_bytes,
            fd_output,
            last_peak_memory_bytes: 0,
            last_fuel_consumed: None,
            last_output_truncated: false,
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

    /// Set the time bound for [`snapshot_state`](Self::snapshot_state),
    /// [`restore_state`](Self::restore_state) and
    /// [`clear_state`](Self::clear_state).
    ///
    /// By default (`None`) it is the execution timeout raised to at least
    /// [`MIN_STATE_OP_TIMEOUT`], or [`DEFAULT_STATE_OP_TIMEOUT`] when there
    /// is no execution timeout. `Some` uses the given duration as is.
    pub fn set_state_op_timeout(&mut self, timeout: Option<Duration>) {
        self.state_op_timeout = timeout;
    }

    /// Let `token` stop [`snapshot_state`](Self::snapshot_state),
    /// [`restore_state`](Self::restore_state) and
    /// [`clear_state`](Self::clear_state), as
    /// [`SessionExecuteBuilder::with_cancellation`] does for an execution.
    ///
    /// A cancelled operation returns [`Error::Cancelled`] and discards the
    /// instance, so the next call resets the session. While a token is set,
    /// CPU-bound guest code in these operations also yields to the async
    /// runtime regularly, so the caller can drop the future.
    pub fn set_state_op_cancellation(&mut self, token: Option<CancellationToken>) {
        self.state_op_cancellation = token;
    }

    /// The time bound applied to state operations; see
    /// [`set_state_op_timeout`](Self::set_state_op_timeout).
    #[must_use]
    pub fn state_op_timeout(&self) -> Duration {
        self.state_op_timeout.unwrap_or_else(|| {
            self.execution_timeout
                .map_or(DEFAULT_STATE_OP_TIMEOUT, |t| t.max(MIN_STATE_OP_TIMEOUT))
        })
    }

    /// Set the fuel limit for all executions in this session.
    ///
    /// When set, executions will be limited to the specified number of
    /// WASM instructions. When fuel runs out, execution traps.
    ///
    /// This can be overridden per-execution using
    /// [`SessionExecuteBuilder::with_fuel_limit`].
    ///
    /// # Arguments
    ///
    /// * `limit` - Optional fuel limit. Pass `None` to disable the limit
    ///   (fuel consumption is still tracked and reported).
    pub fn set_fuel_limit(&mut self, limit: Option<u64>) {
        self.fuel_limit = limit;
    }

    /// Override the name of the variable captured as the structured result for
    /// this session's current instance.
    ///
    /// Normally the name is configured on the parent [`PythonExecutor`] via
    /// [`PythonExecutor::with_result_variable`] and applied at instantiation. This
    /// method sets it on the already-instantiated session instance, e.g. to apply a
    /// per-request override. The override persists across `restore_state` (which
    /// reuses the current instance) but is reset to the executor's configured name
    /// whenever the instance is re-created (e.g. by `reset`/reinstantiation).
    ///
    /// If an earlier call lost the instance (e.g. a state operation timed
    /// out), the session is reset first, as [`execute`](Self::execute) would.
    ///
    /// # Errors
    ///
    /// Returns an error if re-instantiation or the guest call fails.
    pub async fn set_result_variable(&mut self, name: &str) -> Result<(), Error> {
        self.recover_lost_store(&[]).await?;
        let store = self
            .store
            .as_mut()
            .ok_or_else(|| Error::Execution("session store unavailable".to_string()))?;
        let bindings = self
            .bindings
            .as_ref()
            .ok_or_else(|| Error::Execution("session bindings unavailable".to_string()))?;
        let name = name.to_string();
        store
            .run_concurrent(async |accessor| {
                bindings.call_set_result_variable(accessor, name).await
            })
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?
            .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?;
        Ok(())
    }

    /// Get the current fuel limit.
    #[must_use]
    pub fn fuel_limit(&self) -> Option<u64> {
        self.fuel_limit
    }

    /// Peak guest memory, in bytes, reached during the most recent execution.
    ///
    /// Unlike [`ExecutionOutput::peak_memory_bytes`], this is also available
    /// when the execution failed (e.g. a Python exception or a timeout). It is
    /// 0 if the last call failed before the guest ran.
    #[must_use]
    pub fn last_peak_memory_bytes(&self) -> u64 {
        self.last_peak_memory_bytes
    }

    /// Fuel consumed by the most recent execution, also available when the
    /// execution failed. `None` if the last call failed before the guest ran.
    #[must_use]
    pub fn last_fuel_consumed(&self) -> Option<u64> {
        self.last_fuel_consumed
    }

    /// Whether the most recent execution dropped streamed stdout/stderr for
    /// exceeding the per-execution stream budget. Unlike
    /// [`ExecutionOutput::stream_truncated`], it ignores dropped trace events
    /// and is also available when the execution failed.
    #[must_use]
    pub fn last_output_truncated(&self) -> bool {
        self.last_output_truncated
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
    #[allow(clippy::too_many_arguments)]
    async fn execute_internal(
        &mut self,
        code: &str,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
        net_tx: Option<mpsc::Sender<NetRequest>>,
        output_tx: Option<mpsc::UnboundedSender<crate::wasm::OutputRequest>>,
        per_execute_fuel_limit: Option<u64>,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<ExecutionOutput, Error> {
        self.last_peak_memory_bytes = 0;
        self.last_fuel_consumed = None;
        self.last_output_truncated = false;
        crate::error::validate_user_code(code)?;
        if crate::wasm::is_already_cancelled(cancellation_token.as_ref()) {
            return Err(Error::Cancelled);
        }

        let execution_timeout = self.execution_timeout;
        if execution_timeout.is_some() || cancellation_token.is_some() {
            crate::wasm::ensure_epoch_ticker(self.executor.engine())?;
        }

        let start = Instant::now();

        self.recover_lost_store(callbacks).await?;
        // Drop fd writes made outside an execution (e.g. during a snapshot).
        self.fd_output.take(None);

        // Take ownership of store and bindings for async execution
        let mut store = self
            .store
            .take()
            .ok_or_else(|| Error::Execution("Store not available".to_string()))?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| Error::Execution("Bindings not available".to_string()))?;

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
            state.set_net_tx(net_tx);
            state.set_output_tx(output_tx);
            state.set_callbacks(callback_infos);
            state.reset_memory_tracker();
            state.stream_budget = StreamBudget::default();
            // A reused store keeps its ExecutorState across runs; clear any stale
            // suspension flag so it does not misclassify this run's trap.
            state.clear_suspended();
        }

        self.execution_count += 1;

        // Set up fuel for tracking/limiting. Per-execution limit takes precedence
        // over session-level limit. We use u64::MAX for tracking-only mode.
        let fuel_limit = per_execute_fuel_limit.or(self.fuel_limit);
        let initial_fuel = fuel_limit.unwrap_or(u64::MAX);
        store
            .set_fuel(initial_fuel)
            .map_err(|e| Error::Initialization(format!("Failed to set fuel: {e}")))?;

        tracing::debug!(
            code_len = code.len(),
            execution_count = self.execution_count,
            fuel_limit = ?fuel_limit,
            "SessionExecutor: executing Python code"
        );

        // Set up epoch-based deadline if timeout is configured.
        // This allows us to interrupt WASM execution even in tight loops.
        let was_cancelled = Arc::new(AtomicBool::new(false));
        if let Some(token) = cancellation_token.clone() {
            crate::wasm::arm_epoch_cancellation(
                &mut store,
                execution_timeout,
                token,
                Arc::clone(&was_cancelled),
            );
        } else {
            crate::wasm::arm_epoch_deadline(&mut store, execution_timeout);
        }

        // Execute the code.
        // Wrap in tokio::time::timeout so that blocking WASI host calls (e.g. poll_oneoff
        // used by time.sleep) are cancelled when the future is dropped, not just CPU-bound
        // loops caught by epoch interruption.
        //
        // Also race the cancellation token: the epoch callback only observes it
        // while guest code runs, so a guest suspended on a timer or callback
        // would otherwise ignore cancellation until that completes.
        let code_owned = code.to_string();
        let mut async_timeout_elapsed = false;
        let run = store
            .run_concurrent(async |accessor| bindings.call_execute(accessor, code_owned).await);
        let run = async {
            let Some(token) = cancellation_token.as_ref() else {
                return run.await;
            };
            tokio::select! {
                result = run => result,
                () = token.cancelled() => {
                    was_cancelled.store(true, Ordering::Relaxed);
                    Err(wasmtime::Error::msg("cancelled while suspended"))
                }
            }
        };
        let result = if let Some(timeout) = execution_timeout {
            match tokio::time::timeout(timeout, run).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    async_timeout_elapsed = true;
                    Err(wasmtime::Error::msg("async timeout elapsed"))
                }
            }
        } else {
            run.await
        };

        // Disarm the deadline so it can't fire during later calls into the
        // instance (snapshot/restore/clear) made after the timeout window.
        crate::wasm::arm_epoch_deadline(&mut store, None);

        // Clear channels after execution and capture peak memory
        // Capture any suspension reason before the store is restored: a callback
        // that suspended poisons fuel to halt the guest, so the resulting trap
        // must be classified as a suspension rather than fuel-limit exhaustion.
        let suspended_reason = store.data().suspended.clone();

        // Stream and keep the guest's direct fd writes before the output
        // channel is dropped.
        let (fd_stdout, fd_stderr) = self.fd_output.take(store.data().output_tx.as_ref());
        let stream_truncated = store.data().stream_budget.truncated;
        self.last_output_truncated = store.data().stream_budget.output_truncated;

        let peak_memory = {
            let state = store.data_mut();
            state.set_callback_tx(None);
            state.set_trace_tx(None);
            state.set_net_tx(None);
            state.set_output_tx(None);
            state.peak_memory_bytes()
        };

        // Get remaining fuel after execution
        let remaining_fuel = store.get_fuel().unwrap_or(0);

        // Calculate fuel consumed during execution
        let fuel_consumed = Some(initial_fuel.saturating_sub(remaining_fuel));
        self.last_peak_memory_bytes = peak_memory;
        self.last_fuel_consumed = fuel_consumed;

        // A cancelled run was interrupted mid-call, so its instance can't be
        // reused: leave the store out and the next call resets the session.
        if was_cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }

        // Restore store and bindings before handling result
        self.store = Some(store);
        self.bindings = Some(bindings);

        // Classify errors using proper type matching. wasmtime::Error is anyhow::Error,
        // so we downcast to wasmtime::Trap for WASM-level traps (Interrupt, OutOfFuel).
        // The async_timeout_elapsed flag covers blocking WASI host calls (e.g. time.sleep).
        let wasmtime_result = result.map_err(|e| {
            if async_timeout_elapsed
                || e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
            {
                Error::Timeout(execution_timeout.unwrap_or_default())
            } else if e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
                if let Some(reason) = suspended_reason.clone() {
                    Error::Suspended(reason)
                } else {
                    let consumed = initial_fuel.saturating_sub(remaining_fuel);
                    let limit = fuel_limit.unwrap_or(u64::MAX);
                    Error::FuelExhausted { consumed, limit }
                }
            } else {
                Error::Execution(format!("WASM execution error: {e:?}"))
            }
        })?;
        // wasmtime_result is Result<Result<ExecuteOutput, String>, wasmtime::Error>
        // The outer Result is from wasmtime, the inner is the WIT-generated result.
        // The inner Err is the guest's logical execution error — for real input
        // that is always an uncaught Python exception (its traceback).
        let wit_output = wasmtime_result
            .map_err(|e| Error::Execution(format!("WASM execution error: {e:?}")))?
            .map_err(|mut traceback| {
                crate::wasm::append_fd_stderr(&mut traceback, &fd_stderr);
                Error::PythonException(traceback)
            })?;

        let duration = start.elapsed();

        // The guest uses "" as the "absent" sentinel for both result fields.
        let empty_to_none = |s: String| if s.is_empty() { None } else { Some(s) };

        // Note: callback_invocations is 0 here because SessionExecutor doesn't
        // handle callbacks internally - it just passes the channel to the WASM state.
        // Callers that use with_callbacks() should spawn their own callback handler
        // task to count invocations if needed.
        let mut output = ExecutionOutput::new(
            wit_output.stdout,
            wit_output.stderr,
            peak_memory,
            duration,
            0, // Callback invocations tracked by caller's callback handler
            fuel_consumed,
        );
        output.result = empty_to_none(wit_output.result_json);
        output.result_error = empty_to_none(wit_output.result_error);
        output.stream_truncated = stream_truncated;
        output.state_was_reset = std::mem::take(&mut self.state_was_reset);
        output.stdout.extend(fd_stdout);
        output.stderr.extend(fd_stderr);
        Ok(output)
    }

    /// Start over on a fresh instance if a call whose future was dropped
    /// mid-run took the instance with it, rather than failing every later call.
    ///
    /// Returns whether the instance was reset; the next successful execution
    /// then reports [`ExecutionOutput::state_was_reset`].
    ///
    /// # Errors
    ///
    /// Returns an error if re-instantiation fails.
    pub async fn recover_lost_store(
        &mut self,
        callbacks: &[Arc<dyn Callback>],
    ) -> Result<bool, Error> {
        if self.store.is_some() {
            return Ok(false);
        }
        tracing::warn!("previous session call was cancelled mid-run; resetting, state lost");
        self.reset(callbacks).await?;
        self.state_was_reset = true;
        Ok(true)
    }

    /// Get the number of executions performed in this session.
    #[must_use]
    pub fn execution_count(&self) -> u32 {
        self.execution_count
    }

    /// Reset the session to a fresh state.
    ///
    /// This creates a new WASM instance, discarding all previous Python state.
    /// However, VFS storage persists across resets if it was provided at construction.
    ///
    /// # Arguments
    ///
    /// * `callbacks` - Callbacks for the new session
    ///
    /// # Errors
    ///
    /// Returns an error if re-instantiation fails.
    #[tracing::instrument(
        name = "SessionExecutor::reset",
        skip(self, callbacks),
        fields(
            callbacks = callbacks.len(),
            execution_count = self.execution_count,
        )
    )]
    pub async fn reset(&mut self, callbacks: &[Arc<dyn Callback>]) -> Result<(), Error> {
        let callback_infos = build_callback_infos(callbacks);
        let wasi = build_wasi_context(&self.executor, &self.fd_output)?;

        #[cfg(not(feature = "vfs"))]
        let state = ExecutorState::new(
            wasi,
            ResourceTable::new(),
            None,
            None,
            callback_infos,
            MemoryTracker::new(self.memory_limit),
        );

        #[cfg(feature = "vfs")]
        let state = {
            // Reuse existing VFS storage and config so files persist across resets
            let vfs_storage = self.vfs_storage.clone().unwrap_or_else(|| {
                // This is only a defensive fallback (constructors always keep
                // the storage), but retain the configured quota if it is used.
                eryx_vfs::ArcStorage::new(std::sync::Arc::new(
                    eryx_vfs::InMemoryStorage::with_max_bytes(
                        self.vfs_max_bytes.unwrap_or(eryx_vfs::DEFAULT_MAX_BYTES),
                    ),
                ))
            });
            let vfs_config = self.vfs_config.clone().unwrap_or_default();

            ExecutorState::new(
                wasi,
                ResourceTable::new(),
                None,
                None,
                callback_infos,
                MemoryTracker::new(self.memory_limit),
                Some(build_hybrid_vfs_context(
                    &self.executor,
                    vfs_storage,
                    &vfs_config,
                )?),
            )
        };

        // Create new store
        let mut store = Store::new(self.executor.engine(), state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set epoch deadline before instantiation - required when epoch_interruption is enabled
        // in the engine config. We use a very large value (but not u64::MAX to avoid overflow
        // when added to the current epoch).
        store.set_epoch_deadline(u64::MAX / 2);

        // Set initial fuel - required when consume_fuel is enabled in the engine config.
        // We use u64::MAX for tracking-only mode; actual limits are applied per-execution.
        store
            .set_fuel(u64::MAX)
            .map_err(|e| Error::WasmEngine(format!("Failed to set fuel: {e}")))?;

        // Preserve settings across reset
        let execution_timeout = self.execution_timeout;
        let fuel_limit = self.fuel_limit;

        // Instantiate the component
        let bindings = self
            .executor
            .instance_pre()
            .instantiate_async(&mut store)
            .await
            .map_err(|e| Error::WasmEngine(format!("Failed to reinstantiate component: {e}")))?;

        // Re-apply the result-capture variable name on the fresh instance.
        if self.executor.result_variable() != "result" {
            let name = self.executor.result_variable().to_string();
            store
                .run_concurrent(async |accessor| {
                    bindings.call_set_result_variable(accessor, name).await
                })
                .await
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?
                .map_err(|e| Error::WasmEngine(format!("Failed to set result variable: {e}")))?;
        }

        self.store = Some(store);
        self.bindings = Some(bindings);
        self.execution_count = 0;
        self.execution_timeout = execution_timeout;
        self.fuel_limit = fuel_limit;

        Ok(())
    }

    /// Run `run` on this session so that a halted run can't leave it unusable.
    ///
    /// A suspension, timeout, fuel exhaustion, cancellation or trap halts the
    /// guest and poisons the instance. When `callbacks` is non-empty the state
    /// is snapshotted first, costing one [`snapshot_state`](Self::snapshot_state)
    /// per call. If the run then halts (or `suspended` reports that a callback
    /// suspended, which also covers a suspension racing a timeout), the
    /// instance is [`reset`](Self::reset) with `callbacks` and the snapshot
    /// restored, so the session is back where it was before the call,
    /// including [`execution_count`](Self::execution_count).
    ///
    /// With no callbacks nothing can suspend, so no snapshot is taken. If the
    /// snapshot fails (e.g. it exceeds the size limit), a warning is logged and
    /// the run is unprotected. In both cases a halt still resets the instance so
    /// the session stays usable, but its Python state is lost.
    ///
    /// Only serializable globals survive a rollback (as with `snapshot_state`).
    /// VFS/volume writes and network calls made by the run are not undone.
    ///
    /// Returns `run`'s result and whether the session was rolled back.
    ///
    /// # Errors
    ///
    /// Returns an error if the rollback itself fails, which leaves the session
    /// unusable.
    pub async fn run_with_rollback<T>(
        &mut self,
        callbacks: &[Arc<dyn Callback>],
        suspended: impl FnOnce() -> bool,
        run: impl AsyncFnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<(Result<T, Error>, bool), Error> {
        let snapshot = if callbacks.is_empty() {
            None
        } else {
            match self.snapshot_state().await {
                Ok(snapshot) => Some(snapshot),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "session snapshot failed; running without rollback protection"
                    );
                    None
                }
            }
        };
        let execution_count = self.execution_count;

        let result = run(self).await;

        let halted = suspended()
            || matches!(
                result,
                Err(Error::Suspended(_)
                    | Error::Timeout(_)
                    | Error::FuelExhausted { .. }
                    | Error::Cancelled
                    | Error::Execution(_))
            );
        if !halted {
            return Ok((result, false));
        }

        self.reset(callbacks).await?;
        match &snapshot {
            Some(snapshot) => self.restore_state(snapshot).await?,
            None => {
                tracing::warn!("session reset after a halted run without a snapshot; state lost");
                self.state_was_reset = true;
            }
        }
        self.execution_count = execution_count;
        Ok((result, true))
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
    /// - It runs past [`state_op_timeout`](Self::state_op_timeout) or the
    ///   fuel limit (at least [`MIN_STATE_OP_FUEL`]), or is
    ///   [cancelled](Self::set_state_op_cancellation), which also discards
    ///   the instance so the next call resets the session
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").run().await?;
    /// let snapshot = session.snapshot_state().await?;
    /// println!("Snapshot size: {} bytes", snapshot.size());
    /// ```
    pub async fn snapshot_state(&mut self) -> Result<PythonStateSnapshot, Error> {
        tracing::debug!("SessionExecutor: capturing state snapshot");

        let data = self
            .run_state_op("snapshot", async |accessor, bindings| {
                bindings.call_snapshot_state(accessor).await
            })
            .await?
            .map_err(Error::Snapshot)?;

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
    /// - It runs past [`state_op_timeout`](Self::state_op_timeout) or the
    ///   fuel limit (at least [`MIN_STATE_OP_FUEL`]), or is
    ///   [cancelled](Self::set_state_op_cancellation), which also discards
    ///   the instance so the next call resets the session
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
        tracing::debug!(
            size_bytes = snapshot.size(),
            "SessionExecutor: restoring state snapshot"
        );

        let data = snapshot.data().to_vec();
        self.run_state_op("restore", async |accessor, bindings| {
            bindings.call_restore_state(accessor, data).await
        })
        .await?
        .map_err(Error::Snapshot)?;

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
    /// Returns an error if the WASM call fails, or if it runs past
    /// [`state_op_timeout`](Self::state_op_timeout) or the fuel limit (at
    /// least [`MIN_STATE_OP_FUEL`]) or is
    /// [cancelled](Self::set_state_op_cancellation), which also discards the
    /// instance so the next call resets the session.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 1").run().await?;
    /// session.clear_state().await?;
    /// // x is no longer defined
    /// ```
    pub async fn clear_state(&mut self) -> Result<(), Error> {
        tracing::debug!("SessionExecutor: clearing state");

        self.run_state_op("clear state", async |accessor, bindings| {
            bindings.call_clear_state(accessor).await
        })
        .await?;

        tracing::debug!("State cleared");

        Ok(())
    }

    /// Run a state export (snapshot/restore/clear) under
    /// [`state_op_timeout`](Self::state_op_timeout), the session's fuel
    /// limit raised to at least [`MIN_STATE_OP_FUEL`], and the
    /// [state-op cancellation token](Self::set_state_op_cancellation).
    ///
    /// These exports run guest code (e.g. `__reduce__`), so they need a
    /// bound. If it runs out the instance is discarded and the next call
    /// resets the session.
    async fn run_state_op<R>(
        &mut self,
        what: &str,
        call: impl AsyncFnOnce(&Accessor<ExecutorState>, &SandboxBindings) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        let cancellation_token = self.state_op_cancellation.clone();
        if crate::wasm::is_already_cancelled(cancellation_token.as_ref()) {
            return Err(Error::Cancelled);
        }
        self.recover_lost_store(&[]).await?;

        let timeout = self.state_op_timeout();
        crate::wasm::ensure_epoch_ticker(self.executor.engine())?;

        // Take ownership of store and bindings
        let mut store = self
            .store
            .take()
            .ok_or_else(|| Error::WasmEngine("Store not available".to_string()))?;
        let bindings = self
            .bindings
            .take()
            .ok_or_else(|| Error::WasmEngine("Bindings not available".to_string()))?;

        let fuel = self
            .fuel_limit
            .map_or(u64::MAX, |f| f.max(MIN_STATE_OP_FUEL));
        store
            .set_fuel(fuel)
            .map_err(|e| Error::WasmEngine(format!("Failed to set fuel: {e}")))?;
        let was_cancelled = Arc::new(AtomicBool::new(false));
        match cancellation_token.clone() {
            Some(token) => crate::wasm::arm_epoch_cancellation(
                &mut store,
                Some(timeout),
                token,
                Arc::clone(&was_cancelled),
            ),
            None => crate::wasm::arm_epoch_deadline(&mut store, Some(timeout)),
        }

        let run = store.run_concurrent(async |accessor| call(accessor, &bindings).await);
        // As in `execute_internal`: also race the token, which the epoch
        // callback only sees while guest code runs.
        let run = async {
            let Some(token) = cancellation_token.as_ref() else {
                return run.await;
            };
            tokio::select! {
                result = run => result,
                () = token.cancelled() => {
                    was_cancelled.store(true, Ordering::Relaxed);
                    Err(wasmtime::Error::from(wasmtime::Trap::Interrupt))
                }
            }
        };
        let result = tokio::time::timeout(timeout, run)
            .await
            .unwrap_or_else(|_elapsed| Err(wasmtime::Error::from(wasmtime::Trap::Interrupt)));
        crate::wasm::arm_epoch_deadline(&mut store, None);

        let result = result.and_then(|r| r);
        if let Err(e) = &result
            && e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
        {
            // The guest was halted mid-call: drop the instance so the next
            // call starts from a fresh one.
            return Err(crate::wasm::classify_epoch_interrupt(
                was_cancelled.load(Ordering::Relaxed),
                Some(timeout),
            ));
        }
        if let Err(e) = &result
            && e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel)
        {
            let consumed = fuel.saturating_sub(store.get_fuel().unwrap_or(0));
            return Err(Error::FuelExhausted {
                consumed,
                limit: fuel,
            });
        }

        // Restore store and bindings
        self.store = Some(store);
        self.bindings = Some(bindings);

        result.map_err(|e| Error::WasmEngine(format!("WASM {what} error: {e}")))
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
    #[cfg(not(feature = "vfs"))]
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
            net_tx: None,    // Set via with_network() when network handler is running
            output_tx: None, // Set via set_output_tx() when output handler is running
            suspended: None,
            reuse_empty_callbacks: false,
            stream_budget: StreamBudget::default(),
        }
    }

    /// Create a new ExecutorState with the given configuration.
    #[cfg(feature = "vfs")]
    pub(crate) fn new(
        wasi: WasiCtx,
        table: ResourceTable,
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
        callbacks: Vec<HostCallbackInfo>,
        memory_tracker: MemoryTracker,
        hybrid_vfs_ctx: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
    ) -> Self {
        Self {
            wasi,
            table,
            callback_tx,
            trace_tx,
            callbacks,
            memory_tracker,
            net_tx: None,    // Set via with_network() when network handler is running
            output_tx: None, // Set via set_output_tx() when output handler is running
            hybrid_vfs_ctx,
            suspended: None,
            reuse_empty_callbacks: false,
            stream_budget: StreamBudget::default(),
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

    /// Update the network channel for a new execution.
    pub(crate) fn set_net_tx(&mut self, tx: Option<mpsc::Sender<NetRequest>>) {
        self.net_tx = tx;
    }

    /// Update the output streaming channel for a new execution.
    pub(crate) fn set_output_tx(
        &mut self,
        tx: Option<mpsc::UnboundedSender<crate::wasm::OutputRequest>>,
    ) {
        self.output_tx = tx;
    }

    /// Update the available callbacks for a new execution.
    pub(crate) fn set_callbacks(&mut self, callbacks: Vec<HostCallbackInfo>) {
        self.callbacks = callbacks;
    }

    /// Clear any suspension flag left over from a previous execution.
    ///
    /// A reused session [`Store`] keeps its [`ExecutorState`] across runs, so the
    /// suspension flag must be cleared before each execution to avoid a stale
    /// suspension classifying a fresh run's trap.
    pub(crate) fn clear_suspended(&mut self) {
        self.suspended = None;
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
