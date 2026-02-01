//! WebAssembly runtime setup and WIT bindings.
//!
//! This module handles the wasmtime engine configuration, component loading,
//! and host import implementations for running Python code in the sandbox.
//!
//! The `PythonExecutor` uses pre-instantiation (`SandboxPre`) to avoid
//! re-linking on every execution, significantly improving performance.
//!
//! # Pre-compiled Components
//!
//! When the `precompiled` feature is enabled, you can pre-compile the WASM
//! component to native code for faster startup (~50x faster sandbox creation):
//!
//! ```rust,ignore
//! // At build time - compile once:
//! let precompiled = PythonExecutor::precompile_file("runtime.wasm")?;
//! std::fs::write("runtime.cwasm", precompiled)?;
//!
//! // At runtime - load quickly (unsafe, must trust the file):
//! let executor = unsafe { PythonExecutor::from_precompiled_file("runtime.cwasm")? };
//! ```
//!
//! Alternatively, enable the `embedded` feature for a safe API that
//! pre-compiles at build time and embeds the result in the binary.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[cfg(feature = "embedded")]
use crate::cache::{CacheKey, InstancePreCache};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wasmtime::component::{Accessor, Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::callback::Callback;
use crate::error::Error;
use crate::trace::TraceEvent;

/// Request to invoke a callback from Python code.
#[derive(Debug)]
pub struct CallbackRequest {
    /// Name of the callback to invoke.
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments_json: String,
    /// Channel to send the response back.
    pub response_tx: oneshot::Sender<std::result::Result<String, String>>,
}

/// Request to report a trace event from Python code.
#[derive(Debug, Clone)]
pub struct TraceRequest {
    /// Line number in the source code.
    pub lineno: u32,
    /// Event type as JSON.
    pub event_json: String,
    /// Optional context data as JSON.
    pub context_json: String,
}

/// Request for a network operation from Python code.
#[derive(Debug)]
pub enum NetRequest {
    // TCP operations
    /// Connect to a host over TCP.
    TcpConnect {
        host: String,
        port: u16,
        response_tx: oneshot::Sender<Result<u32, crate::net::TcpError>>,
    },
    /// Read from a TCP connection.
    TcpRead {
        handle: u32,
        len: u32,
        response_tx: oneshot::Sender<Result<Vec<u8>, crate::net::TcpError>>,
    },
    /// Write to a TCP connection.
    TcpWrite {
        handle: u32,
        data: Vec<u8>,
        response_tx: oneshot::Sender<Result<u32, crate::net::TcpError>>,
    },
    /// Close a TCP connection.
    TcpClose { handle: u32 },

    // TLS operations
    /// Upgrade a TCP connection to TLS.
    TlsUpgrade {
        tcp_handle: u32,
        hostname: String,
        response_tx: oneshot::Sender<Result<u32, crate::net::TlsError>>,
    },
    /// Read from a TLS connection.
    TlsRead {
        handle: u32,
        len: u32,
        response_tx: oneshot::Sender<Result<Vec<u8>, crate::net::TlsError>>,
    },
    /// Write to a TLS connection.
    TlsWrite {
        handle: u32,
        data: Vec<u8>,
        response_tx: oneshot::Sender<Result<u32, crate::net::TlsError>>,
    },
    /// Close a TLS connection.
    TlsClose { handle: u32 },
}

/// Callback info for introspection (internal type to avoid conflicts with generated code).
#[derive(Debug, Clone)]
pub struct HostCallbackInfo {
    /// Unique name for this callback.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for expected arguments.
    pub parameters_schema_json: String,
}

/// Output from executing Python code in the WASM sandbox.
///
/// This struct is `#[non_exhaustive]` to allow adding new fields in the future
/// without breaking semver compatibility.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExecutionOutput {
    /// Captured stdout from the Python execution.
    pub stdout: String,
    /// Captured stderr from the Python execution.
    pub stderr: String,
    /// Peak memory usage in bytes during execution.
    pub peak_memory_bytes: u64,
    /// Execution duration.
    pub duration: Duration,
    /// Number of callback invocations during execution.
    pub callback_invocations: u32,
    /// Fuel consumed during execution (if fuel tracking is enabled).
    ///
    /// This measures the number of WASM instructions executed. Always present
    /// when the engine has fuel consumption enabled.
    pub fuel_consumed: Option<u64>,
}

impl ExecutionOutput {
    /// Create a new execution output.
    #[must_use]
    pub fn new(
        stdout: String,
        stderr: String,
        peak_memory_bytes: u64,
        duration: Duration,
        callback_invocations: u32,
        fuel_consumed: Option<u64>,
    ) -> Self {
        Self {
            stdout,
            stderr,
            peak_memory_bytes,
            duration,
            callback_invocations,
            fuel_consumed,
        }
    }
}

/// CPU feature level for AOT compilation.
///
/// These levels correspond to x86-64 microarchitecture feature tiers.
/// Using a lower level produces more portable binaries at the cost of
/// potentially slower execution.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{PythonExecutor, CpuFeatureLevel};
///
/// // Compile for Fly.io (x86-64-v3, no AVX-512)
/// let cwasm = PythonExecutor::precompile_with_options(
///     &wasm_bytes,
///     None,  // native target
///     Some(CpuFeatureLevel::X86_64_V3),
/// )?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuFeatureLevel {
    /// Baseline x86_64: SSE2 only. Maximum compatibility (~2003+ CPUs).
    X86_64,
    /// x86-64-v2: SSE4.2, POPCNT, SSSE3 (~2008+ CPUs, Nehalem/Westmere era).
    X86_64v2,
    /// x86-64-v3: AVX2, FMA, BMI1/2 (~2013+ CPUs, Haswell era). No AVX-512.
    /// **Recommended for Fly.io** and other cloud VMs.
    X86_64v3,
    /// x86-64-v4: AVX-512 (~2017+ CPUs, Skylake-X era).
    X86_64v4,
    /// Use host CPU features for best performance. Not portable.
    #[default]
    Native,
}

impl CpuFeatureLevel {
    /// Parse from string (e.g., "x86-64-v3" or "native").
    ///
    /// Returns `None` if the string doesn't match a known level.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "x86-64" | "x86-64-v1" => Some(Self::X86_64),
            "x86-64-v2" => Some(Self::X86_64v2),
            "x86-64-v3" => Some(Self::X86_64v3),
            "x86-64-v4" => Some(Self::X86_64v4),
            "native" => Some(Self::Native),
            _ => None,
        }
    }

    /// Convert to the string representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86-64",
            Self::X86_64v2 => "x86-64-v2",
            Self::X86_64v3 => "x86-64-v3",
            Self::X86_64v4 => "x86-64-v4",
            Self::Native => "native",
        }
    }
}

impl std::fmt::Display for CpuFeatureLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for CpuFeatureLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            format!(
                "Unknown CPU feature level '{}'. Valid values: x86-64, x86-64-v2, x86-64-v3, x86-64-v4, native",
                s
            )
        })
    }
}

/// Tracks memory usage during WASM execution.
///
/// This struct implements `ResourceLimiter` to intercept memory growth
/// requests and track the peak memory usage. It can optionally enforce
/// a memory limit.
#[derive(Debug)]
pub struct MemoryTracker {
    /// Peak memory usage observed (in bytes).
    peak_memory_bytes: AtomicU64,
    /// Optional memory limit (in bytes). If set, memory growth beyond this limit will fail.
    memory_limit: Option<u64>,
}

impl MemoryTracker {
    /// Create a new memory tracker with an optional limit.
    #[must_use]
    pub fn new(memory_limit: Option<u64>) -> Self {
        Self {
            peak_memory_bytes: AtomicU64::new(0),
            memory_limit,
        }
    }

    /// Get the peak memory usage observed so far (in bytes).
    #[must_use]
    pub fn peak_memory_bytes(&self) -> u64 {
        self.peak_memory_bytes.load(Ordering::Relaxed)
    }

    /// Reset the peak memory tracker to zero.
    pub fn reset(&self) {
        self.peak_memory_bytes.store(0, Ordering::Relaxed);
    }
}

impl ResourceLimiter for MemoryTracker {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        // Track peak memory usage
        let desired_u64 = desired as u64;
        self.peak_memory_bytes
            .fetch_max(desired_u64, Ordering::Relaxed);

        // Check against our configured limit
        if self.memory_limit.is_some_and(|limit| desired_u64 > limit) {
            return Ok(false);
        }

        // Check against WASM-declared maximum
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }

        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        // Allow table growth up to the declared maximum
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        Ok(true)
    }
}

// Generate bindings from the WIT file
//
// Network functions are declared as regular sync `func` in WIT (not `async func`).
// We use the `async` flag for network imports to generate `func_wrap_async` bindings.
// This gives us fiber-based async: the host can await on async operations, but from
// the guest's perspective the calls are blocking. This requires `Config::async_support`
// but NOT `Config::wasm_component_model_async`.
wasmtime::component::bindgen!({
    path: "../eryx-runtime/wit",
    imports: {
        // TCP network operations - fiber-based async (blocking to guest, async on host)
        "eryx:net/tcp.connect": async,
        "eryx:net/tcp.read": async,
        "eryx:net/tcp.write": async,
        "eryx:net/tcp.close": async,
        // TLS network operations - fiber-based async (blocking to guest, async on host)
        "eryx:net/tls.upgrade": async,
        "eryx:net/tls.read": async,
        "eryx:net/tls.write": async,
        "eryx:net/tls.close": async,
    },
});

/// State for a single execution, implementing WASI and callback channels.
pub struct ExecutorState {
    /// WASI context for the execution.
    pub(crate) wasi: WasiCtx,
    /// Resource table for WASI.
    pub(crate) table: ResourceTable,
    /// Channel to send callback requests to the host.
    pub(crate) callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    /// Channel to send trace events to the host.
    pub(crate) trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    /// Available callbacks for introspection.
    pub(crate) callbacks: Vec<HostCallbackInfo>,
    /// Memory usage tracker.
    pub(crate) memory_tracker: MemoryTracker,
    /// Channel to send network requests to the handler.
    pub(crate) net_tx: Option<mpsc::Sender<NetRequest>>,
    /// Hybrid virtual filesystem context (when vfs feature is enabled).
    /// Routes /data/* to VFS storage, other paths to real filesystem.
    #[cfg(feature = "vfs")]
    pub(crate) hybrid_vfs_ctx: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::InMemoryStorage>>,
}

impl std::fmt::Debug for ExecutorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ExecutorState");
        debug
            .field("wasi", &"<WasiCtx>")
            .field("table", &"<ResourceTable>")
            .field("callback_tx", &self.callback_tx.is_some())
            .field("trace_tx", &self.trace_tx.is_some())
            .field("callbacks", &self.callbacks.len())
            .field(
                "peak_memory_bytes",
                &self.memory_tracker.peak_memory_bytes(),
            )
            .field("net_tx", &self.net_tx.is_some());
        #[cfg(feature = "vfs")]
        debug.field("hybrid_vfs_ctx", &self.hybrid_vfs_ctx.is_some());
        debug.finish()
    }
}

impl WasiView for ExecutorState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[cfg(feature = "vfs")]
impl eryx_vfs::HybridVfsView for ExecutorState {
    type Storage = eryx_vfs::InMemoryStorage;

    #[allow(clippy::expect_used)]
    fn hybrid_vfs(&mut self) -> eryx_vfs::HybridVfsState<'_, Self::Storage> {
        eryx_vfs::HybridVfsState::new(
            self.hybrid_vfs_ctx
                .as_mut()
                .expect("Hybrid VFS not configured for this executor"),
            &mut self.table,
        )
    }
}

// Host implementation of the WIT-generated sandbox imports trait.
impl SandboxImportsWithStore for HasSelf<ExecutorState> {
    /// Invoke a callback by name with JSON arguments (async).
    fn invoke<T>(
        accessor: &Accessor<T, Self>,
        name: String,
        arguments_json: String,
    ) -> impl ::core::future::Future<Output = Result<String, String>> + Send {
        tracing::debug!(
            callback = %name,
            args_len = arguments_json.len(),
            "Python invoking callback"
        );

        async move {
            if let Some(tx) = accessor.with(|mut access| access.get().callback_tx.clone()) {
                // Create oneshot channel for receiving the response
                let (response_tx, response_rx) = oneshot::channel();

                let request = CallbackRequest {
                    name: name.clone(),
                    arguments_json,
                    response_tx,
                };

                // Send request to the callback handler
                if tx.send(request).await.is_err() {
                    Err("Callback channel closed".to_string())
                } else {
                    // Wait for response
                    response_rx
                        .await
                        .unwrap_or_else(|_| Err("Callback response channel closed".to_string()))
                }
            } else {
                // No callback channel - return error
                Err(format!("Callback '{name}' not available (no handler)"))
            }
        }
    }
}

impl SandboxImports for ExecutorState {
    /// List all available callbacks for introspection.
    fn list_callbacks(&mut self) -> Vec<CallbackInfo> {
        self.callbacks
            .iter()
            .map(|cb| CallbackInfo {
                name: cb.name.clone(),
                description: cb.description.clone(),
                parameters_schema_json: cb.parameters_schema_json.clone(),
            })
            .collect()
    }

    /// Report a trace event to the host.
    fn report_trace(&mut self, lineno: u32, event_json: String, context_json: String) {
        if let Some(tx) = &self.trace_tx {
            let request = TraceRequest {
                lineno,
                event_json,
                context_json,
            };
            // Fire-and-forget - trace events are not critical
            let _ = tx.send(request);
        }
    }
}

// Import the WIT-generated network module types
use self::eryx::net::tcp;
use self::eryx::net::tls;

// Convert our TcpError to the WIT-generated TcpError type.
fn to_wit_tcp_error(e: crate::net::TcpError) -> tcp::TcpError {
    use crate::net::TcpError as E;
    match e {
        E::ConnectionRefused => tcp::TcpError::ConnectionRefused,
        E::ConnectionReset => tcp::TcpError::ConnectionReset,
        E::TimedOut => tcp::TcpError::TimedOut,
        E::HostNotFound => tcp::TcpError::HostNotFound,
        E::IoError(msg) => tcp::TcpError::IoError(msg),
        E::NotPermitted(msg) => tcp::TcpError::NotPermitted(msg),
        E::InvalidHandle => tcp::TcpError::InvalidHandle,
    }
}

// Convert our TlsError to the WIT-generated TlsError type.
fn to_wit_tls_error(e: crate::net::TlsError) -> tls::TlsError {
    use crate::net::TlsError as E;
    match e {
        E::Tcp(tcp_err) => tls::TlsError::Tcp(to_wit_tcp_error(tcp_err)),
        E::HandshakeFailed(msg) => tls::TlsError::HandshakeFailed(msg),
        E::CertificateError(msg) => tls::TlsError::CertificateError(msg),
        E::InvalidHandle => tls::TlsError::InvalidHandle,
    }
}

// ============================================================================
// TCP Host Implementation (fiber-based async)
// ============================================================================

impl tcp::Host for ExecutorState {
    async fn connect(&mut self, host: String, port: u16) -> Result<u32, tcp::TcpError> {
        tracing::debug!(host = %host, port, "TCP connect requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpConnect {
            host,
            port,
            response_tx,
        };

        // Send request - fiber will suspend if channel is full
        tx.send(request)
            .await
            .map_err(|_| tcp::TcpError::IoError("network handler channel closed".into()))?;

        // Await response - fiber suspends until response arrives
        response_rx
            .await
            .map_err(|_| tcp::TcpError::IoError("network response channel closed".into()))?
            .map_err(to_wit_tcp_error)
    }

    async fn read(&mut self, handle: u32, len: u32) -> Result<Vec<u8>, tcp::TcpError> {
        tracing::trace!(handle, len, "TCP read requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpRead {
            handle,
            len,
            response_tx,
        };

        tx.send(request)
            .await
            .map_err(|_| tcp::TcpError::IoError("network handler channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| tcp::TcpError::IoError("network response channel closed".into()))?
            .map_err(to_wit_tcp_error)
    }

    async fn write(&mut self, handle: u32, data: Vec<u8>) -> Result<u32, tcp::TcpError> {
        tracing::trace!(handle, len = data.len(), "TCP write requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpWrite {
            handle,
            data,
            response_tx,
        };

        tx.send(request)
            .await
            .map_err(|_| tcp::TcpError::IoError("network handler channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| tcp::TcpError::IoError("network response channel closed".into()))?
            .map_err(to_wit_tcp_error)
    }

    async fn close(&mut self, handle: u32) {
        tracing::debug!(handle, "TCP close requested");
        if let Some(ref tx) = self.net_tx {
            // Fire-and-forget for close
            let _ = tx.send(NetRequest::TcpClose { handle }).await;
        }
    }
}

// ============================================================================
// TLS Host Implementation (fiber-based async)
// ============================================================================

impl tls::Host for ExecutorState {
    async fn upgrade(&mut self, tcp_handle: u32, hostname: String) -> Result<u32, tls::TlsError> {
        tracing::debug!(tcp_handle, hostname = %hostname, "TLS upgrade requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsUpgrade {
            tcp_handle,
            hostname,
            response_tx,
        };

        tx.send(request).await.map_err(|_| {
            tls::TlsError::Tcp(tcp::TcpError::IoError(
                "network handler channel closed".into(),
            ))
        })?;

        response_rx
            .await
            .map_err(|_| {
                tls::TlsError::Tcp(tcp::TcpError::IoError(
                    "network response channel closed".into(),
                ))
            })?
            .map_err(to_wit_tls_error)
    }

    async fn read(&mut self, handle: u32, len: u32) -> Result<Vec<u8>, tls::TlsError> {
        tracing::trace!(handle, len, "TLS read requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsRead {
            handle,
            len,
            response_tx,
        };

        tx.send(request).await.map_err(|_| {
            tls::TlsError::Tcp(tcp::TcpError::IoError(
                "network handler channel closed".into(),
            ))
        })?;

        response_rx
            .await
            .map_err(|_| {
                tls::TlsError::Tcp(tcp::TcpError::IoError(
                    "network response channel closed".into(),
                ))
            })?
            .map_err(to_wit_tls_error)
    }

    async fn write(&mut self, handle: u32, data: Vec<u8>) -> Result<u32, tls::TlsError> {
        tracing::trace!(handle, len = data.len(), "TLS write requested");

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsWrite {
            handle,
            data,
            response_tx,
        };

        tx.send(request).await.map_err(|_| {
            tls::TlsError::Tcp(tcp::TcpError::IoError(
                "network handler channel closed".into(),
            ))
        })?;

        response_rx
            .await
            .map_err(|_| {
                tls::TlsError::Tcp(tcp::TcpError::IoError(
                    "network response channel closed".into(),
                ))
            })?
            .map_err(to_wit_tls_error)
    }

    async fn close(&mut self, handle: u32) {
        tracing::debug!(handle, "TLS close requested");
        if let Some(ref tx) = self.net_tx {
            // Fire-and-forget for close
            let _ = tx.send(NetRequest::TlsClose { handle }).await;
        }
    }
}

/// Builder for configuring and executing Python code.
///
/// Created by [`PythonExecutor::execute`]. Use the builder methods to
/// configure callbacks, tracing, memory limits, and timeouts, then call
/// [`run`](Self::run) to execute.
///
/// # Example
///
/// ```rust,ignore
/// let output = executor
///     .execute("print('hello')")
///     .with_callbacks(&callbacks, callback_tx)
///     .with_timeout(Duration::from_secs(5))
///     .run()
///     .await?;
/// ```
pub struct ExecuteBuilder<'a> {
    executor: &'a PythonExecutor,
    code: String,
    callbacks: Vec<Arc<dyn Callback>>,
    callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    net_tx: Option<mpsc::Sender<NetRequest>>,
    memory_limit: Option<u64>,
    execution_timeout: Option<Duration>,
    cancellation_token: Option<CancellationToken>,
    fuel_limit: Option<u64>,
}

impl std::fmt::Debug for ExecuteBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteBuilder")
            .field("code_len", &self.code.len())
            .field("callbacks_count", &self.callbacks.len())
            .field("has_callback_tx", &self.callback_tx.is_some())
            .field("has_trace_tx", &self.trace_tx.is_some())
            .field("has_net_tx", &self.net_tx.is_some())
            .field("memory_limit", &self.memory_limit)
            .field("execution_timeout", &self.execution_timeout)
            .field("has_cancellation_token", &self.cancellation_token.is_some())
            .field("fuel_limit", &self.fuel_limit)
            .finish_non_exhaustive()
    }
}

impl<'a> ExecuteBuilder<'a> {
    /// Create a new execute builder.
    fn new(executor: &'a PythonExecutor, code: impl Into<String>) -> Self {
        Self {
            executor,
            code: code.into(),
            callbacks: Vec::new(),
            callback_tx: None,
            trace_tx: None,
            net_tx: None,
            memory_limit: None,
            execution_timeout: None,
            cancellation_token: None,
            fuel_limit: None,
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

    /// Set the maximum memory usage in bytes.
    #[must_use]
    pub fn with_memory_limit(mut self, limit: u64) -> Self {
        self.memory_limit = Some(limit);
        self
    }

    /// Set the execution timeout.
    ///
    /// Uses epoch-based interruption to interrupt even tight loops
    /// like `while True: pass`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.execution_timeout = Some(timeout);
        self
    }

    /// Set a cancellation token for external cancellation support.
    ///
    /// When the token is cancelled, the execution will be interrupted
    /// using epoch-based interruption, similar to timeouts.
    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Set the maximum fuel (instructions) allowed for execution.
    ///
    /// Fuel provides fine-grained, deterministic execution bounds at the
    /// instruction level. When fuel runs out, execution traps.
    ///
    /// If not set, fuel defaults to `u64::MAX` for tracking-only mode,
    /// where fuel consumption is still measured and reported.
    #[must_use]
    pub fn with_fuel_limit(mut self, fuel: u64) -> Self {
        self.fuel_limit = Some(fuel);
        self
    }

    /// Execute the Python code with the configured options.
    ///
    /// # Errors
    ///
    /// Returns an error if execution fails, times out, or exceeds memory/fuel limits.
    pub async fn run(self) -> std::result::Result<ExecutionOutput, Error> {
        self.executor
            .execute_internal(
                &self.code,
                &self.callbacks,
                self.callback_tx,
                self.trace_tx,
                self.net_tx,
                self.memory_limit,
                self.execution_timeout,
                self.cancellation_token,
                self.fuel_limit,
                #[cfg(feature = "vfs")]
                None,
            )
            .await
    }
}

/// The Python executor that manages the WASM runtime.
///
/// This struct uses pre-instantiation (`SandboxPre`) to perform as much
/// work as possible upfront. The expensive operations (parsing WASM,
/// compiling, linking) happen once during construction. Each `execute()`
/// call only needs to create a new store and instantiate from the
/// pre-compiled template, which is much faster.
pub struct PythonExecutor {
    /// The wasmtime engine (shared configuration).
    engine: Engine,
    /// Pre-instantiated component - linking is already done.
    instance_pre: SandboxPre<ExecutorState>,
    /// Path to the Python standard library directory.
    /// Required for the eryx-wasm-runtime to initialize Python.
    python_stdlib_path: Option<PathBuf>,
    /// Paths to Python package directories.
    /// Each will be mounted at `/site-packages-N` and added to PYTHONPATH.
    python_site_packages_paths: Vec<PathBuf>,
}

impl std::fmt::Debug for PythonExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonExecutor")
            .field("engine", &"<wasmtime::Engine>")
            .field("instance_pre", &"<SandboxPre>")
            .field("python_stdlib_path", &self.python_stdlib_path)
            .field(
                "python_site_packages_paths",
                &self.python_site_packages_paths,
            )
            .finish_non_exhaustive()
    }
}

impl PythonExecutor {
    /// Get a reference to the wasmtime engine.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to the pre-instantiated component.
    #[must_use]
    pub fn instance_pre(&self) -> &SandboxPre<ExecutorState> {
        &self.instance_pre
    }

    /// Get the Python stdlib path if configured.
    #[must_use]
    pub fn python_stdlib_path(&self) -> Option<&PathBuf> {
        self.python_stdlib_path.as_ref()
    }

    /// Get the Python site-packages paths.
    #[must_use]
    pub fn python_site_packages_paths(&self) -> &[PathBuf] {
        &self.python_site_packages_paths
    }

    /// Set the path to the Python standard library directory.
    ///
    /// This is required when using the eryx-wasm-runtime (Rust/CPython FFI based).
    /// The directory should contain the extracted Python stdlib (e.g., from
    /// componentize-py's python-lib.tar.zst).
    ///
    /// The stdlib will be mounted at `/python-stdlib` inside the WASM sandbox.
    #[must_use]
    pub fn with_python_stdlib(mut self, path: impl Into<PathBuf>) -> Self {
        self.python_stdlib_path = Some(path.into());
        self
    }

    /// Add a path to Python packages directory.
    ///
    /// The first directory will be mounted at `/site-packages` inside the WASM sandbox
    /// (for compatibility with preinit). Additional directories are mounted at
    /// `/site-packages-1`, `/site-packages-2`, etc. All paths are added to Python's
    /// import path. Can be called multiple times.
    #[must_use]
    pub fn with_site_packages(mut self, path: impl Into<PathBuf>) -> Self {
        self.python_site_packages_paths.push(path.into());
        self
    }

    /// Get or create the global shared wasmtime Engine.
    ///
    /// The Engine is thread-safe and automatically shared across all `PythonExecutor`
    /// instances. Sharing an Engine saves memory and startup time since the JIT
    /// compiler configuration and compiled code cache are shared.
    ///
    /// This is called automatically by `from_binary`, `from_file`, etc.
    /// You typically don't need to call this directly.
    ///
    /// # Errors
    ///
    /// Returns an error if engine creation fails (only on first call).
    pub fn shared_engine() -> std::result::Result<Engine, Error> {
        use std::sync::OnceLock;
        static SHARED_ENGINE: OnceLock<Engine> = OnceLock::new();

        // Fast path: engine already initialized
        if let Some(engine) = SHARED_ENGINE.get() {
            return Ok(engine.clone());
        }

        // Slow path: create engine (may race with other threads)
        let engine = Self::create_engine()?;
        // get_or_init handles the race - only one engine is kept
        Ok(SHARED_ENGINE.get_or_init(|| engine).clone())
    }

    /// Create a new executor by loading a WASM component from bytes.
    ///
    /// This performs all expensive operations upfront:
    /// - Parsing and validating the WASM component
    /// - Compiling to native code
    /// - Linking WASI and sandbox imports
    /// - Creating a pre-instantiated template
    ///
    /// Uses the global shared Engine automatically for memory efficiency.
    ///
    /// Subsequent calls to `execute()` will be fast because they only
    /// need to instantiate from the pre-compiled template.
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be loaded or the
    /// wasmtime engine cannot be configured.
    #[tracing::instrument(
        name = "PythonExecutor::from_binary",
        skip(wasm_bytes),
        fields(wasm_bytes_len = wasm_bytes.len())
    )]
    pub fn from_binary(wasm_bytes: &[u8]) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;
        let component =
            Component::from_binary(&engine, wasm_bytes).map_err(Error::WasmComponent)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        Ok(Self {
            engine,
            instance_pre,
            python_stdlib_path: None,
            python_site_packages_paths: Vec::new(),
        })
    }

    /// Create a new executor by loading a WASM component from a file.
    ///
    /// This performs all expensive operations upfront (see `from_binary`).
    /// Uses the global shared Engine automatically.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the WASM component
    /// cannot be loaded.
    #[tracing::instrument(
        name = "PythonExecutor::from_file",
        fields(path = %path.as_ref().display())
    )]
    pub fn from_file(path: impl AsRef<std::path::Path>) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;
        let component =
            Component::from_file(&engine, path.as_ref()).map_err(Error::WasmComponent)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        Ok(Self {
            engine,
            instance_pre,
            python_stdlib_path: None,
            python_site_packages_paths: Vec::new(),
        })
    }

    /// Create a new executor by loading a pre-compiled component from bytes.
    ///
    /// This is much faster than `from_binary` because it skips compilation.
    /// The pre-compiled bytes must have been created by `precompile()` with
    /// a compatible engine configuration.
    /// Uses the global shared Engine automatically.
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate pre-compiled
    /// components for safety. Only use this with pre-compiled bytes you control
    /// and trust. Using untrusted bytes can lead to arbitrary code execution.
    ///
    /// # Errors
    ///
    /// Returns an error if the pre-compiled bytes are invalid or incompatible
    /// with the current engine configuration.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled(precompiled_bytes: &[u8]) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;
        // SAFETY: Caller guarantees the precompiled bytes are trusted and were
        // created by `precompile()` with a compatible engine configuration.
        let component = unsafe { Component::deserialize(&engine, precompiled_bytes) }
            .map_err(Error::WasmComponent)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        Ok(Self {
            engine,
            instance_pre,
            python_stdlib_path: None,
            python_site_packages_paths: Vec::new(),
        })
    }

    /// Create a new executor by loading a pre-compiled component from a file.
    ///
    /// This is much faster than `from_file` because it skips compilation.
    /// Uses the global shared Engine automatically.
    /// The file must have been created by `precompile()` with a compatible
    /// engine configuration.
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate pre-compiled
    /// components for safety. Only use this with files you control and trust.
    /// Using untrusted files can lead to arbitrary code execution.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the pre-compiled component
    /// is invalid or incompatible with the current engine configuration.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled_file(
        path: impl AsRef<std::path::Path>,
    ) -> std::result::Result<Self, Error> {
        // Delegate to the internal method without a cache key
        // This means it won't use or populate the InstancePreCache
        #[allow(unsafe_code)]
        unsafe {
            Self::from_precompiled_file_internal(path.as_ref(), None)
        }
    }

    /// Create a new executor by loading a pre-compiled component from a file,
    /// with optional caching via [`InstancePreCache`].
    ///
    /// When a `cache_key` is provided:
    /// - First checks the global [`InstancePreCache`] for a cached `SandboxPre`
    /// - On cache hit, returns immediately without disk I/O (~0ms)
    /// - On cache miss, loads from file and stores in cache for future use
    ///
    /// This is the internal method used by [`Self::from_embedded_runtime`] and
    /// the sandbox builder for cached executor creation.
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate pre-compiled
    /// components for safety. Only use this with files you control and trust.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the pre-compiled component
    /// is invalid or incompatible with the current engine configuration.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_precompiled_file_with_key(
        path: impl AsRef<std::path::Path>,
        cache_key: CacheKey,
    ) -> std::result::Result<Self, Error> {
        #[allow(unsafe_code)]
        unsafe {
            Self::from_precompiled_file_internal(path.as_ref(), Some(cache_key))
        }
    }

    /// Internal implementation for loading from precompiled file with optional caching.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    unsafe fn from_precompiled_file_internal(
        path: &std::path::Path,
        cache_key: Option<CacheKey>,
    ) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;

        // Check InstancePreCache if we have a cache key
        if let Some(ref key) = cache_key
            && let Some(instance_pre) = InstancePreCache::global().get(key)
        {
            return Ok(Self {
                engine,
                instance_pre,
                python_stdlib_path: None,
                python_site_packages_paths: Vec::new(),
            });
        }

        // Cache miss - load from file and create instance_pre
        // SAFETY: Caller guarantees the precompiled file is trusted and was
        // created by `precompile()` with a compatible engine configuration.
        #[allow(unsafe_code)]
        let component =
            unsafe { Component::deserialize_file(&engine, path) }.map_err(Error::WasmComponent)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        // Store in cache if we have a key
        if let Some(key) = cache_key {
            InstancePreCache::global().put(key, instance_pre.clone());
        }

        Ok(Self {
            engine,
            instance_pre,
            python_stdlib_path: None,
            python_site_packages_paths: Vec::new(),
        })
    }

    /// Create a new executor from the embedded runtime.
    ///
    /// This is the fastest way to create an executor:
    /// - Uses the global shared [`Engine`]
    /// - Uses the global [`InstancePreCache`] with a sentinel key
    /// - Only loads from disk on first call; subsequent calls return cached instance
    ///
    /// The executor is created without Python stdlib or site-packages paths.
    /// Use [`Self::with_python_stdlib`] and [`Self::with_site_packages`] to
    /// configure paths after creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded resources cannot be extracted or
    /// the component cannot be loaded.
    #[cfg(feature = "embedded")]
    #[tracing::instrument(name = "PythonExecutor::from_embedded_runtime")]
    pub fn from_embedded_runtime() -> std::result::Result<Self, Error> {
        let cache_key = CacheKey::embedded_runtime();

        // Get embedded resources (extracts stdlib on first call)
        let resources = crate::embedded::EmbeddedResources::get()?;
        let stdlib_path = Some(resources.stdlib_path.clone());

        // Check InstancePreCache first (fast path)
        if let Some(instance_pre) = InstancePreCache::global().get(&cache_key) {
            return Ok(Self {
                engine: Self::shared_engine()?,
                instance_pre,
                python_stdlib_path: stdlib_path,
                python_site_packages_paths: Vec::new(),
            });
        }

        // Cache miss - load from embedded resources
        // SAFETY: The embedded runtime was pre-compiled at build time from our own
        // trusted runtime.wasm, so we know it's safe to deserialize.
        #[allow(unsafe_code)]
        let mut executor =
            unsafe { Self::from_precompiled_file_with_key(resources.runtime(), cache_key)? };

        executor.python_stdlib_path = stdlib_path;
        Ok(executor)
    }

    /// Create a new executor from a cached `SandboxPre`.
    ///
    /// This is used internally when an `InstancePreCache` hit occurs.
    /// The `SandboxPre` must have been created with a compatible engine.
    #[cfg(all(feature = "embedded", feature = "native-extensions"))]
    pub(crate) fn from_cached_instance_pre(
        instance_pre: SandboxPre<ExecutorState>,
    ) -> std::result::Result<Self, Error> {
        Ok(Self {
            engine: Self::shared_engine()?,
            instance_pre,
            python_stdlib_path: None,
            python_site_packages_paths: Vec::new(),
        })
    }

    /// Pre-compile the WASM component to native code for faster loading.
    ///
    /// The returned bytes can be saved to a file (conventionally with `.cwasm`
    /// extension) and later loaded with `from_precompiled` or `from_precompiled_file`.
    ///
    /// # Benefits
    ///
    /// - Faster startup: Skip compilation when loading
    /// - Lower memory: Pre-compiled code can be lazily mmap'd from disk
    /// - Smaller runtime: Can build without compiler for production
    ///
    /// # Errors
    ///
    /// Returns an error if pre-compilation fails.
    pub fn precompile(wasm_bytes: &[u8]) -> std::result::Result<Vec<u8>, Error> {
        let engine = Self::create_engine()?;
        engine
            .precompile_component(wasm_bytes)
            .map_err(|e| Error::WasmEngine(format!("Failed to precompile component: {e}")))
    }

    /// Pre-compile a WASM component file to native code.
    ///
    /// Convenience method that reads the file and calls `precompile`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or pre-compilation fails.
    pub fn precompile_file(
        path: impl AsRef<std::path::Path>,
    ) -> std::result::Result<Vec<u8>, Error> {
        let wasm_bytes = std::fs::read(path.as_ref())
            .map_err(|e| Error::WasmEngine(format!("Failed to read WASM file: {e}")))?;
        Self::precompile(&wasm_bytes)
    }

    /// Pre-compile the WASM component to native code for a specific target.
    ///
    /// This allows creating pre-compiled artifacts for a different architecture
    /// (cross-compilation).
    ///
    /// # Target values
    ///
    /// Target must be a valid target triple (see `target_lexicon::Triple`):
    /// - `"x86_64-unknown-linux-gnu"` - Linux x86_64
    /// - `"x86_64-apple-darwin"` - macOS x86_64
    /// - `"aarch64-unknown-linux-gnu"` - Linux ARM64
    /// - `"aarch64-apple-darwin"` - macOS ARM64 (Apple Silicon)
    /// - `None` - Native CPU (fastest, but may not be portable)
    ///
    /// # Disabling CPU features
    ///
    /// To disable specific CPU features like AVX-512 for more portable builds,
    /// set the `ERYX_CRANELIFT_FLAGS` environment variable:
    ///
    /// ```bash
    /// ERYX_CRANELIFT_FLAGS=has_avx512f=false,has_avx512bw=false,has_avx512dq=false,has_avx512vl=false
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the target is invalid or pre-compilation fails.
    pub fn precompile_with_target(
        wasm_bytes: &[u8],
        target: Option<&str>,
    ) -> std::result::Result<Vec<u8>, Error> {
        let engine = Self::create_engine_with_target(target)?;
        engine
            .precompile_component(wasm_bytes)
            .map_err(|e| Error::WasmEngine(format!("Failed to precompile component: {e}")))
    }

    /// Pre-compile a WASM component file to native code for a specific target.
    ///
    /// Convenience method that reads the file and calls `precompile_with_target`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, target is invalid, or pre-compilation fails.
    pub fn precompile_file_with_target(
        path: impl AsRef<std::path::Path>,
        target: Option<&str>,
    ) -> std::result::Result<Vec<u8>, Error> {
        let wasm_bytes = std::fs::read(path.as_ref())
            .map_err(|e| Error::WasmEngine(format!("Failed to read WASM file: {e}")))?;
        Self::precompile_with_target(&wasm_bytes, target)
    }

    /// Pre-compile the WASM component to native code with explicit CPU feature control.
    ///
    /// This provides fine-grained control over both the target architecture and CPU feature
    /// level without requiring environment variables.
    ///
    /// # Arguments
    ///
    /// * `wasm_bytes` - The WASM component bytes to precompile
    /// * `target` - Optional target triple (e.g., "aarch64-unknown-linux-gnu"). None uses native.
    /// * `cpu_features` - CPU feature level to target. Use `CpuFeatureLevel::X86_64v3` for Fly.io.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::{PythonExecutor, CpuFeatureLevel};
    ///
    /// // Compile for Fly.io (x86-64-v3, no AVX-512)
    /// let cwasm = PythonExecutor::precompile_with_options(
    ///     &wasm_bytes,
    ///     None,  // native target triple
    ///     CpuFeatureLevel::X86_64v3,
    /// )?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the target is invalid or pre-compilation fails.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    pub fn precompile_with_options(
        wasm_bytes: &[u8],
        target: Option<&str>,
        cpu_features: CpuFeatureLevel,
    ) -> std::result::Result<Vec<u8>, Error> {
        let engine = Self::create_engine_with_options(target, cpu_features)?;
        engine
            .precompile_component(wasm_bytes)
            .map_err(|e| Error::WasmEngine(format!("Failed to precompile component: {e}")))
    }

    /// Create a configured wasmtime engine.
    ///
    /// Version identifier for engine configuration.
    ///
    /// Bump this when making changes to `create_engine()` that affect
    /// precompiled component compatibility (e.g., enabling epoch interruption).
    /// This helps CI caches invalidate when the engine config changes.
    ///
    /// Version history:
    /// - v1: Initial configuration
    /// - v2: Added epoch_interruption(true) for execution timeouts
    /// - v3: Added consume_fuel(true) for instruction tracking/limiting
    pub const ENGINE_CONFIG_VERSION: u32 = 3;

    /// Create a configured wasmtime engine.
    ///
    /// Uses copy-on-write heap images to defer memory initialization
    /// from instantiation time to first write, improving startup performance.
    fn create_engine() -> std::result::Result<Engine, Error> {
        Self::create_engine_with_target(None)
    }

    /// Create a configured wasmtime engine with an optional target triple.
    ///
    /// When `target` is `None`, compiles for the native CPU (fastest but may use
    /// instructions like AVX-512 that aren't available on all machines).
    ///
    /// When `target` is `Some`, compiles for the specified target triple. Examples:
    /// - `"x86_64-unknown-linux-gnu"` - Linux x86_64
    /// - `"x86_64-apple-darwin"` - macOS x86_64
    /// - `"aarch64-unknown-linux-gnu"` - Linux ARM64
    /// - `"aarch64-apple-darwin"` - macOS ARM64 (Apple Silicon)
    ///
    /// Also checks the `ERYX_TARGET` environment variable if no target is provided.
    ///
    /// # CPU Feature Levels
    ///
    /// Set `ERYX_CPU_FEATURES` to a preset level:
    /// - `x86-64` - Baseline x86_64 (SSE2 only, most compatible)
    /// - `x86-64-v2` - SSE4.2, POPCNT, SSSE3 (Nehalem/Westmere era, ~2008+)
    /// - `x86-64-v3` - AVX2, BMI1/2, FMA (Haswell era, ~2013+, no AVX-512)
    /// - `x86-64-v4` - AVX-512 (Skylake-X era, ~2017+)
    /// - `native` - Use host CPU features (default)
    ///
    /// # Custom Cranelift Flags
    ///
    /// For fine-grained control, set `ERYX_CRANELIFT_FLAGS` to a comma-separated
    /// list of `flag=value` pairs. Example:
    /// `ERYX_CRANELIFT_FLAGS=has_avx512f=false,has_avx512bw=false`
    fn create_engine_with_target(target: Option<&str>) -> std::result::Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Enable component model async for the `invoke` callback function.
        // The invoke function is async because Python code awaits on it.
        // TCP/TLS functions are sync `func` in WIT but use fiber-based async
        // on the host (via `async` bindgen flag) - they appear blocking to guest.
        config.wasm_component_model_async(true);
        config.async_support(true);

        // Enable epoch-based interruption for execution timeouts.
        // This allows us to interrupt WASM execution even in tight loops
        // that don't yield to the async runtime (e.g., `while True: pass`).
        config.epoch_interruption(true);

        // Enable fuel consumption for instruction tracking and limiting.
        // Fuel provides fine-grained, deterministic execution bounds at the
        // instruction level. Even when no limit is set, fuel consumption is
        // tracked and reported for billing/metering purposes.
        config.consume_fuel(true);

        // Enable copy-on-write heap images for faster instantiation
        // This defers memory initialization from instantiation time to first write
        config.memory_init_cow(true);

        // Optimize for smaller generated code (slight runtime perf tradeoff)
        // This reduces .cwasm file sizes and memory footprint
        config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);

        // Reduce async stack size from default 2 MiB to 512 KiB
        // Python scripts don't need deep call stacks
        config.async_stack_size(512 * 1024);

        // Configure target triple for cross-compilation or portable builds.
        // Check explicit parameter first, then environment variable, then use native.
        let effective_target = target
            .map(|s| s.to_string())
            .or_else(|| std::env::var("ERYX_TARGET").ok());

        if let Some(ref target_str) = effective_target {
            config
                .target(target_str)
                .map_err(|e| Error::WasmEngine(format!("Invalid target '{target_str}': {e}")))?;
        }

        // Apply CPU feature level preset and custom Cranelift flags.
        // These require unsafe code, so they're only available with embedded or preinit features.
        #[cfg(any(feature = "embedded", feature = "preinit"))]
        Self::apply_cpu_feature_flags(&mut config)?;

        Engine::new(&config).map_err(|e| Error::WasmEngine(e.to_string()))
    }

    /// Create a configured wasmtime engine with explicit CPU feature control.
    ///
    /// This provides an alternative to environment variables for controlling CPU features,
    /// making it easier to use in CLI tools and avoiding global state.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    fn create_engine_with_options(
        target: Option<&str>,
        cpu_features: CpuFeatureLevel,
    ) -> std::result::Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.async_support(true);
        config.epoch_interruption(true);
        config.consume_fuel(true);
        config.memory_init_cow(true);
        config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
        config.async_stack_size(512 * 1024);

        // Configure target triple
        if let Some(target_str) = target {
            config
                .target(target_str)
                .map_err(|e| Error::WasmEngine(format!("Invalid target '{target_str}': {e}")))?;
        }

        // Apply CPU feature level
        Self::apply_cpu_feature_level(&mut config, cpu_features)?;

        Engine::new(&config).map_err(|e| Error::WasmEngine(e.to_string()))
    }

    /// Apply CPU feature level preset to the wasmtime config.
    ///
    /// This disables CPU instructions that are above the requested level,
    /// producing more portable binaries.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn apply_cpu_feature_level(
        config: &mut Config,
        level: CpuFeatureLevel,
    ) -> std::result::Result<(), Error> {
        // AVX-512 feature flags to disable for x86-64-v3 and below
        const AVX512_FLAGS: &[&str] = &[
            "has_avx512bitalg",
            "has_avx512dq",
            "has_avx512f",
            "has_avx512vbmi",
            "has_avx512vl",
        ];

        match level {
            CpuFeatureLevel::X86_64 => {
                // Baseline: disable everything above SSE2
                // SAFETY: These are valid Cranelift flags for x86
                unsafe {
                    config.cranelift_flag_set("has_sse3", "false");
                    config.cranelift_flag_set("has_ssse3", "false");
                    config.cranelift_flag_set("has_sse41", "false");
                    config.cranelift_flag_set("has_sse42", "false");
                    config.cranelift_flag_set("has_avx", "false");
                    config.cranelift_flag_set("has_avx2", "false");
                    config.cranelift_flag_set("has_fma", "false");
                    config.cranelift_flag_set("has_bmi1", "false");
                    config.cranelift_flag_set("has_bmi2", "false");
                    config.cranelift_flag_set("has_lzcnt", "false");
                    config.cranelift_flag_set("has_popcnt", "false");
                    for flag in AVX512_FLAGS {
                        config.cranelift_flag_set(flag, "false");
                    }
                }
            }
            CpuFeatureLevel::X86_64v2 => {
                // SSE4.2, POPCNT, SSSE3 - disable AVX and above
                // SAFETY: These are valid Cranelift flags for x86
                unsafe {
                    config.cranelift_flag_set("has_avx", "false");
                    config.cranelift_flag_set("has_avx2", "false");
                    config.cranelift_flag_set("has_fma", "false");
                    config.cranelift_flag_set("has_bmi1", "false");
                    config.cranelift_flag_set("has_bmi2", "false");
                    for flag in AVX512_FLAGS {
                        config.cranelift_flag_set(flag, "false");
                    }
                }
            }
            CpuFeatureLevel::X86_64v3 => {
                // AVX2, FMA, BMI1/2 - disable AVX-512 only
                // This is what Fly.io shared CPUs support (AMD EPYC)
                // SAFETY: These are valid Cranelift flags for x86
                unsafe {
                    for flag in AVX512_FLAGS {
                        config.cranelift_flag_set(flag, "false");
                    }
                }
            }
            CpuFeatureLevel::X86_64v4 | CpuFeatureLevel::Native => {
                // Full features - nothing to disable
            }
        }

        Ok(())
    }

    /// Apply CPU feature level presets and custom Cranelift flags.
    ///
    /// This handles `ERYX_CPU_FEATURES` (preset levels) and `ERYX_CRANELIFT_FLAGS` (custom flags).
    /// Requires `embedded` or `preinit` feature for the unsafe Cranelift flag APIs.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn apply_cpu_feature_flags(config: &mut Config) -> std::result::Result<(), Error> {
        // Apply CPU feature level preset from environment variable.
        // This provides an easy way to target specific x86-64 microarchitecture levels.
        if let Ok(level) = std::env::var("ERYX_CPU_FEATURES") {
            // AVX-512 feature flags to disable for x86-64-v3 and below
            // Note: Only flags that exist in Cranelift's x86 settings are listed here
            const AVX512_FLAGS: &[&str] = &[
                "has_avx512bitalg",
                "has_avx512dq",
                "has_avx512f",
                "has_avx512vbmi",
                "has_avx512vl",
            ];

            match level.as_str() {
                "x86-64" | "x86-64-v1" => {
                    // Baseline: disable everything above SSE2
                    // SAFETY: These are valid Cranelift flags for x86
                    unsafe {
                        config.cranelift_flag_set("has_sse3", "false");
                        config.cranelift_flag_set("has_ssse3", "false");
                        config.cranelift_flag_set("has_sse41", "false");
                        config.cranelift_flag_set("has_sse42", "false");
                        config.cranelift_flag_set("has_avx", "false");
                        config.cranelift_flag_set("has_avx2", "false");
                        config.cranelift_flag_set("has_fma", "false");
                        config.cranelift_flag_set("has_bmi1", "false");
                        config.cranelift_flag_set("has_bmi2", "false");
                        config.cranelift_flag_set("has_lzcnt", "false");
                        config.cranelift_flag_set("has_popcnt", "false");
                        for flag in AVX512_FLAGS {
                            config.cranelift_flag_set(flag, "false");
                        }
                    }
                }
                "x86-64-v2" => {
                    // SSE4.2, POPCNT, SSSE3 - disable AVX and above
                    // SAFETY: These are valid Cranelift flags for x86
                    unsafe {
                        config.cranelift_flag_set("has_avx", "false");
                        config.cranelift_flag_set("has_avx2", "false");
                        config.cranelift_flag_set("has_fma", "false");
                        config.cranelift_flag_set("has_bmi1", "false");
                        config.cranelift_flag_set("has_bmi2", "false");
                        for flag in AVX512_FLAGS {
                            config.cranelift_flag_set(flag, "false");
                        }
                    }
                }
                "x86-64-v3" => {
                    // AVX2, FMA, BMI1/2 - disable AVX-512 only
                    // This is what Fly.io shared CPUs support (AMD EPYC)
                    // SAFETY: These are valid Cranelift flags for x86
                    unsafe {
                        for flag in AVX512_FLAGS {
                            config.cranelift_flag_set(flag, "false");
                        }
                    }
                }
                "x86-64-v4" | "native" => {
                    // Full features - nothing to disable
                }
                other => {
                    return Err(Error::WasmEngine(format!(
                        "Unknown CPU feature level '{other}'. Valid values: \
                         x86-64, x86-64-v2, x86-64-v3, x86-64-v4, native"
                    )));
                }
            }
        }

        // Apply custom Cranelift flags from environment variable.
        // Format: ERYX_CRANELIFT_FLAGS=flag1=value1,flag2=value2
        if let Ok(flags) = std::env::var("ERYX_CRANELIFT_FLAGS") {
            for flag_spec in flags.split(',') {
                let flag_spec = flag_spec.trim();
                if flag_spec.is_empty() {
                    continue;
                }
                // SAFETY: User-provided flags - they take responsibility for correctness.
                // Invalid flags will cause Engine::new() to fail with an error.
                unsafe {
                    if let Some((flag, value)) = flag_spec.split_once('=') {
                        config.cranelift_flag_set(flag.trim(), value.trim());
                    } else {
                        config.cranelift_flag_enable(flag_spec);
                    }
                }
            }
        }

        Ok(())
    }

    /// Create a pre-instantiated component with all imports linked.
    ///
    /// This does the expensive linking work once, so that `execute()` can
    /// quickly instantiate from the template.
    #[tracing::instrument(name = "PythonExecutor::create_instance_pre", skip(engine, component))]
    fn create_instance_pre(
        engine: &Engine,
        component: &Component,
    ) -> std::result::Result<SandboxPre<ExecutorState>, Error> {
        let mut linker = Linker::<ExecutorState>::new(engine);

        // Add WASI support (p2 = preview 2)
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| Error::WasmEngine(format!("Failed to add WASI to linker: {e}")))?;

        // Add hybrid VFS filesystem support (overrides WASI filesystem bindings)
        // The hybrid VFS routes:
        // - /data/* paths to VFS storage (sandboxed in-memory/KV store)
        // - Other paths (like /python-stdlib/*) to real filesystem via WASI
        // This allows Python to access its stdlib while providing sandboxed storage.
        #[cfg(feature = "vfs")]
        {
            // Allow shadowing to override WASI filesystem bindings
            linker.allow_shadowing(true);
            eryx_vfs::add_hybrid_vfs_to_linker(&mut linker).map_err(|e| {
                Error::WasmEngine(format!("Failed to add hybrid VFS to linker: {e}"))
            })?;
            linker.allow_shadowing(false);
        }

        // Add sandbox bindings (includes TCP/TLS interfaces)
        tracing::debug!("Adding sandbox bindings to linker");
        Sandbox::add_to_linker::<_, HasSelf<ExecutorState>>(&mut linker, |state| state)
            .map_err(|e| Error::WasmEngine(format!("Failed to add sandbox to linker: {e}")))?;
        tracing::debug!("Sandbox bindings added successfully");

        // Create pre-instantiated component
        // This validates that all imports are satisfied and prepares for fast instantiation
        let instance_pre = linker
            .instantiate_pre(component)
            .map_err(|e| Error::WasmEngine(format!("Failed to create instance_pre: {e}")))?;

        // Wrap in SandboxPre for typed access to exports
        SandboxPre::new(instance_pre)
            .map_err(|e| Error::WasmEngine(format!("Failed to create SandboxPre: {e}")))
    }

    /// Execute Python code with a fluent builder API.
    ///
    /// This is the primary way to execute code. Use the returned builder
    /// to configure callbacks, tracing, memory limits, and timeouts.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Simple execution
    /// let output = executor.execute("print('hello')").run().await?;
    ///
    /// // With all options
    /// let output = executor
    ///     .execute("result = await my_callback()")
    ///     .with_callbacks(&callbacks, callback_tx)
    ///     .with_tracing(trace_tx)
    ///     .with_memory_limit(64 * 1024 * 1024)
    ///     .with_timeout(Duration::from_secs(5))
    ///     .run()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn execute(&self, code: impl Into<String>) -> ExecuteBuilder<'_> {
        ExecuteBuilder::new(self, code)
    }

    /// Internal execute implementation with all parameters.
    ///
    /// This is called by [`ExecuteBuilder::run`].
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "PythonExecutor::execute_internal",
        skip_all,
        fields(
            code_len = code.len(),
            callbacks = callbacks.len(),
            memory_limit = ?memory_limit,
            timeout = ?execution_timeout,
            fuel_limit = ?fuel_limit,
        )
    )]
    async fn execute_internal(
        &self,
        code: &str,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
        net_tx: Option<mpsc::Sender<NetRequest>>,
        memory_limit: Option<u64>,
        execution_timeout: Option<Duration>,
        cancellation_token: Option<CancellationToken>,
        fuel_limit: Option<u64>,
        #[cfg(feature = "vfs")] vfs_storage: Option<std::sync::Arc<eryx_vfs::InMemoryStorage>>,
    ) -> std::result::Result<ExecutionOutput, Error> {
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
        // The first site-packages is mounted at /site-packages (for preinit compatibility)
        // Additional ones are mounted at /site-packages-1, /site-packages-2, etc.
        let mut pythonpath_parts = Vec::new();
        if self.python_stdlib_path.is_some() {
            pythonpath_parts.push("/python-stdlib".to_string());
        }
        for i in 0..self.python_site_packages_paths.len() {
            if i == 0 {
                pythonpath_parts.push("/site-packages".to_string());
            } else {
                pythonpath_parts.push(format!("/site-packages-{i}"));
            }
        }

        // Mount Python stdlib if configured (required for eryx-wasm-runtime)
        if let Some(ref stdlib_path) = self.python_stdlib_path {
            // PYTHONHOME tells Python where to find the standard library
            wasi_builder.env("PYTHONHOME", "/python-stdlib");
            wasi_builder
                .preopened_dir(
                    stdlib_path,
                    "/python-stdlib",
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| {
                    Error::Initialization(format!("Failed to mount Python stdlib: {e}"))
                })?;
        }

        // Set PYTHONPATH for all configured paths (stdlib and/or site-packages)
        if !pythonpath_parts.is_empty() {
            wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
        }

        // Mount each site-packages directory
        // First one at /site-packages (for preinit compatibility), rest at /site-packages-N
        for (i, site_packages_path) in self.python_site_packages_paths.iter().enumerate() {
            let mount_path = if i == 0 {
                "/site-packages".to_string()
            } else {
                format!("/site-packages-{i}")
            };
            wasi_builder
                .preopened_dir(
                    site_packages_path,
                    &mount_path,
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| Error::Initialization(format!("Failed to mount {mount_path}: {e}")))?;
        }

        let wasi = wasi_builder.build();

        // Build hybrid VFS context when vfs feature is enabled.
        // The hybrid VFS routes /data/* to VFS storage while passing through
        // other paths (like /python-stdlib/*) to the real filesystem.
        // When no VFS storage is provided, we still configure the hybrid VFS
        // with real filesystem preopens to satisfy the linker bindings.
        #[cfg(feature = "vfs")]
        let hybrid_vfs_ctx = {
            // Use provided storage or create an empty in-memory storage
            let storage = vfs_storage
                .unwrap_or_else(|| std::sync::Arc::new(eryx_vfs::InMemoryStorage::new()));
            let mut ctx = eryx_vfs::HybridVfsCtx::new(storage);

            // Add a writable /data directory backed by VFS storage
            ctx.add_vfs_preopen(
                "/data",
                eryx_vfs::DirPerms::all(),
                eryx_vfs::FilePerms::all(),
            );

            // Add Python stdlib as read-only real filesystem preopen
            if let Some(ref stdlib_path) = self.python_stdlib_path
                && let Err(e) = ctx.add_real_preopen_path(
                    "/python-stdlib",
                    stdlib_path,
                    eryx_vfs::DirPerms::READ,
                    eryx_vfs::FilePerms::READ,
                )
            {
                tracing::warn!("Failed to add Python stdlib to hybrid VFS: {e}");
            }

            // Add site-packages directories as read-only real filesystem preopens
            for (i, site_packages_path) in self.python_site_packages_paths.iter().enumerate() {
                let mount_path = if i == 0 {
                    "/site-packages".to_string()
                } else {
                    format!("/site-packages-{i}")
                };
                if let Err(e) = ctx.add_real_preopen_path(
                    &mount_path,
                    site_packages_path,
                    eryx_vfs::DirPerms::READ,
                    eryx_vfs::FilePerms::READ,
                ) {
                    tracing::warn!("Failed to add {mount_path} to hybrid VFS: {e}");
                }
            }

            Some(ctx)
        };

        let state = ExecutorState {
            wasi,
            table: ResourceTable::new(),
            callback_tx,
            trace_tx,
            callbacks: callback_infos,
            memory_tracker: MemoryTracker::new(memory_limit),
            net_tx,
            #[cfg(feature = "vfs")]
            hybrid_vfs_ctx,
        };

        // Create store for this execution
        let mut store = Store::new(&self.engine, state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Set a high epoch deadline for instantiation - we don't want to timeout during
        // Python initialization, only during user code execution.
        store.set_epoch_deadline(u64::MAX / 2);

        // Set up fuel for tracking/limiting. We use u64::MAX for tracking-only mode
        // when no explicit limit is set. Fuel is consumed per WASM instruction.
        let initial_fuel = fuel_limit.unwrap_or(u64::MAX);
        store
            .set_fuel(initial_fuel)
            .map_err(|e| Error::Initialization(format!("Failed to set fuel: {e}")))?;

        // Instantiate from the pre-compiled template (includes Python initialization)
        let bindings = self
            .instance_pre
            .instantiate_async(&mut store)
            .await
            .map_err(Error::WasmComponent)?;

        tracing::debug!(code_len = code.len(), "Executing Python code");

        // Now set up epoch-based deadline for execution timeout and/or cancellation.
        // This is done AFTER instantiation so the timeout only applies to user code execution,
        // not Python initialization.
        const EPOCH_TICK_MS: u64 = 10;

        // Track whether execution was cancelled (vs timed out)
        let was_cancelled = Arc::new(AtomicBool::new(false));

        // Set up epoch ticker if we have a timeout or cancellation token
        let epoch_ticker = if execution_timeout.is_some() || cancellation_token.is_some() {
            // Set deadline based on timeout, or use a moderate value for cancellation-only
            if let Some(timeout) = execution_timeout {
                let ticks_until_timeout = timeout.as_millis() as u64 / EPOCH_TICK_MS;
                let ticks = ticks_until_timeout.max(1);
                store.set_epoch_deadline(ticks);
            } else {
                // No timeout but we have cancellation - set a reachable deadline.
                // When cancelled, we bump epoch by more than this to trigger interrupt.
                const CANCELLATION_DEADLINE: u64 = 10000;
                store.set_epoch_deadline(CANCELLATION_DEADLINE);
            }

            // Configure the store to trap when the epoch deadline is reached
            store.epoch_deadline_trap();

            // Spawn a thread to increment the engine's epoch periodically.
            // We use a std::thread instead of tokio::spawn because the WASM
            // execution may block the tokio runtime, preventing async tasks
            // from running.
            let engine = self.engine.clone();
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_flag_clone = Arc::clone(&stop_flag);
            let was_cancelled_clone = Arc::clone(&was_cancelled);
            let cancel_token = cancellation_token.clone();
            std::thread::spawn(move || {
                while !stop_flag_clone.load(Ordering::Relaxed) {
                    // Check for cancellation
                    if let Some(ref token) = cancel_token
                        && token.is_cancelled()
                    {
                        was_cancelled_clone.store(true, Ordering::Relaxed);
                        // Bump epoch to exceed CANCELLATION_DEADLINE and trigger interrupt
                        for _ in 0..10001 {
                            engine.increment_epoch();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                    engine.increment_epoch();
                }
            });
            Some(stop_flag)
        } else {
            // No timeout and no cancellation - set a very high deadline that won't be reached
            // (but not u64::MAX to avoid overflow when added to current epoch)
            store.set_epoch_deadline(u64::MAX / 2);
            store.epoch_deadline_trap();
            None::<Arc<AtomicBool>>
        };

        // Call the async execute export
        let code_owned = code.to_string();

        // run_concurrent returns Result<R, Error> where R is the closure's return type
        let wasmtime_result = store
            .run_concurrent(async |accessor| bindings.call_execute(accessor, code_owned).await)
            .await;

        // Stop the epoch ticker thread if it was running
        if let Some(stop_flag) = epoch_ticker {
            stop_flag.store(true, Ordering::Relaxed);
        }

        // Check for epoch deadline exceeded (timeout or cancellation) or fuel exhaustion
        let wasmtime_result = wasmtime_result.map_err(|e| {
            let err_str = format!("{e:?}");
            if err_str.contains("epoch deadline") || err_str.contains("wasm trap: interrupt") {
                if was_cancelled.load(Ordering::Relaxed) {
                    Error::Cancelled
                } else {
                    Error::Timeout(execution_timeout.unwrap_or_default())
                }
            } else if err_str.contains("fuel") || err_str.contains("out of fuel") {
                // Calculate how much fuel was consumed before exhaustion
                let remaining = store.get_fuel().unwrap_or(0);
                let consumed = initial_fuel.saturating_sub(remaining);
                let limit = fuel_limit.unwrap_or(u64::MAX);
                Error::FuelExhausted { consumed, limit }
            } else {
                Error::Execution(format!("WASM execution error: {e:?}"))
            }
        })?;

        // wasmtime_result is Result<Result<ExecuteOutput, String>, wasmtime::Error> from the WIT layer
        // The outer Result is from wasmtime, the inner is the WIT-generated result
        // where ExecuteOutput is the WIT-generated record with stdout and stderr
        let wit_output = wasmtime_result
            .map_err(|e| Error::Execution(format!("WASM execution error: {e:?}")))?
            .map_err(Error::Execution)?;

        // Get peak memory from the store before it's dropped
        let peak_memory_bytes = store.data().memory_tracker.peak_memory_bytes();

        // Calculate fuel consumed during execution
        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let fuel_consumed = Some(initial_fuel.saturating_sub(remaining_fuel));

        // Note: callback_invocations is 0 here because PythonExecutor doesn't
        // handle callbacks internally - it just passes the channel to the WASM state.
        // Higher-level APIs like Sandbox track callback invocations in their own
        // callback handler tasks.
        Ok(ExecutionOutput::new(
            wit_output.stdout,
            wit_output.stderr,
            peak_memory_bytes,
            Duration::ZERO, // Duration tracked by higher-level APIs (Sandbox, Session)
            0,              // Callback invocations tracked by higher-level APIs
            fuel_consumed,
        ))
    }
}

/// Parse a trace request into a `TraceEvent`.
///
/// # Errors
///
/// Returns an error if the event JSON cannot be parsed.
pub fn parse_trace_event(request: &TraceRequest) -> std::result::Result<TraceEvent, Error> {
    let event_data: serde_json::Value = serde_json::from_str(&request.event_json)
        .map_err(|e| Error::Serialization(e.to_string()))?;

    let event_type = event_data
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let context: Option<serde_json::Value> = if request.context_json.is_empty() {
        None
    } else {
        serde_json::from_str(&request.context_json).ok()
    };

    let kind = match event_type {
        "line" => crate::trace::TraceEventKind::Line,
        "call" => {
            let function = event_data
                .get("function")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string();
            crate::trace::TraceEventKind::Call { function }
        }
        "return" => {
            let function = event_data
                .get("function")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string();
            crate::trace::TraceEventKind::Return { function }
        }
        "exception" => {
            let message = event_data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            crate::trace::TraceEventKind::Exception { message }
        }
        "callback_start" => {
            let name = event_data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string();
            crate::trace::TraceEventKind::CallbackStart { name }
        }
        "callback_end" => {
            let name = event_data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_string();
            // Duration would need to be tracked by the host
            crate::trace::TraceEventKind::CallbackEnd {
                name,
                duration_ms: 0,
            }
        }
        _ => crate::trace::TraceEventKind::Line,
    };

    Ok(TraceEvent {
        lineno: request.lineno,
        event: kind,
        context,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_trace_event_line() {
        let request = TraceRequest {
            lineno: 42,
            event_json: r#"{"type": "line"}"#.to_string(),
            context_json: String::new(),
        };

        let event = parse_trace_event(&request).unwrap();
        assert_eq!(event.lineno, 42);
        assert!(matches!(event.event, crate::trace::TraceEventKind::Line));
    }

    #[test]
    fn test_parse_trace_event_call() {
        let request = TraceRequest {
            lineno: 10,
            event_json: r#"{"type": "call", "function": "my_func"}"#.to_string(),
            context_json: String::new(),
        };

        let event = parse_trace_event(&request).unwrap();
        assert_eq!(event.lineno, 10);
        if let crate::trace::TraceEventKind::Call { function } = &event.event {
            assert_eq!(function, "my_func");
        } else {
            panic!("Expected Call event");
        }
    }

    #[test]
    fn test_parse_trace_event_callback() {
        let request = TraceRequest {
            lineno: 0,
            event_json: r#"{"type": "callback_start", "name": "http.get"}"#.to_string(),
            context_json: r#"{"url": "https://example.com"}"#.to_string(),
        };

        let event = parse_trace_event(&request).unwrap();
        assert!(event.context.is_some());
        if let crate::trace::TraceEventKind::CallbackStart { name } = &event.event {
            assert_eq!(name, "http.get");
        } else {
            panic!("Expected CallbackStart event");
        }
    }

    /// Test that all CPU feature level presets use valid Cranelift flags.
    ///
    /// This test ensures that if someone adds a new flag name that doesn't exist
    /// in Cranelift, the test will fail at CI time rather than at runtime.
    #[test]
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn test_cpu_feature_levels_use_valid_cranelift_flags() {
        use std::env;

        // Test each preset level by creating an engine with it
        for level in [
            "x86-64",
            "x86-64-v1",
            "x86-64-v2",
            "x86-64-v3",
            "x86-64-v4",
            "native",
        ] {
            // SAFETY: This test runs single-threaded and we clean up after ourselves.
            // The env vars are only read during engine creation in this same thread.
            unsafe {
                env::set_var("ERYX_CPU_FEATURES", level);
                env::remove_var("ERYX_CRANELIFT_FLAGS");
                env::remove_var("ERYX_TARGET");
            }

            // Creating the engine will fail if any flag name is invalid
            let result = PythonExecutor::create_engine_with_target(None);
            assert!(
                result.is_ok(),
                "CPU feature level '{level}' failed to create engine: {:?}",
                result.err()
            );
        }

        // Clean up
        // SAFETY: Same as above - single-threaded test cleanup.
        unsafe {
            env::remove_var("ERYX_CPU_FEATURES");
        }
    }
}
