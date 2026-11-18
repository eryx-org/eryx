//! WebAssembly runtime setup and WIT bindings.
//!
//! This module handles the wasmtime engine configuration, component loading,
//! and host import implementations for running Python code in the sandbox.
//!
//! The `Executor` uses pre-instantiation (`SandboxPre`) to avoid
//! re-linking on every execution, significantly improving performance.
//!
//! # Pre-compiled Components
//!
//! When the `precompiled` feature is enabled, you can pre-compile the WASM
//! component to native code for faster startup (~50x faster sandbox creation):
//!
//! ```rust,ignore
//! // At build time - compile once:
//! let precompiled = Executor::precompile_file("runtime-python.wasm")?;
//! std::fs::write("runtime-python.cwasm", precompiled)?;
//!
//! // At runtime - load quickly (unsafe, must trust the file):
//! let executor = unsafe { Executor::from_precompiled_file("runtime-python.cwasm")? };
//! ```
//!
//! Alternatively, enable the `embedded` feature for a safe API that
//! pre-compiles at build time and embeds the result in the binary.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(feature = "embedded")]
use crate::cache::{CacheKey, InstancePreCache};

use crate::CancellationToken;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, Component, HasSelf, Linker, ResourceTable};
use wasmtime::{
    AsContextMut, Config, Enabled, Engine, InstanceAllocationStrategy, ModuleVersionStrategy,
    PoolingAllocationConfig, ResourceLimiter, Store, UpdateDeadline,
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::callback::Callback;
use crate::error::{Error, ErrorKind};
use crate::fork_local::ForkLocal;
use crate::language::Language;
use crate::trace::TraceEvent;
use crate::warm::{WarmInstance, WarmPool};

/// Interval between increments of the process-wide epoch ticker.
pub(crate) const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// How many instructions a guest runs between points where wasmtime writes its
/// fuel count back to the store.
///
/// Compiled code keeps the count in a register and writes it back only at
/// calls and at some traps. Without this, a run stopped by an epoch interrupt
/// (a timeout or a cancellation) reads back the count from the last write-back,
/// however long it ran. Refuelling every `FUEL_SYNC_INTERVAL` instructions
/// bounds that error by the interval. It also yields to the async runtime.
const FUEL_SYNC_INTERVAL: u64 = 10_000_000;

/// Make `store` write its fuel count back every [`FUEL_SYNC_INTERVAL`]
/// instructions, so the count is readable after the guest is interrupted.
pub(crate) fn sync_fuel_periodically<T>(store: &mut Store<T>) -> std::result::Result<(), Error> {
    store
        .fuel_async_yield_interval(Some(FUEL_SYNC_INTERVAL))
        .map_err(|e| ErrorKind::WasmEngine(format!("Failed to set fuel sync interval: {e}")).into())
}

/// Convert an execution timeout into a number of epoch ticks.
pub(crate) fn epoch_ticks(timeout: Duration) -> u64 {
    let ticks = timeout.as_millis() / EPOCH_TICK_INTERVAL.as_millis();
    u64::try_from(ticks).unwrap_or(u64::MAX / 2).max(1)
}

/// A shared value whose failed initialization attempts can be retried.
///
/// Successful reads are lock-free. The mutex serializes only initialization
/// attempts, allowing a later caller to retry after a transient error or panic.
pub(crate) struct RetryableOnce<T> {
    value: OnceLock<T>,
    init_lock: Mutex<()>,
}

impl<T> RetryableOnce<T> {
    pub(crate) const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            init_lock: Mutex::new(()),
        }
    }

    /// The value, if it has been initialized.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    pub(crate) fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub(crate) fn get_or_try_init<E>(
        &self,
        init: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<&T, E> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }

        let _guard = self
            .init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = self.value.get() {
            return Ok(value);
        }

        let value = init()?;
        Ok(self.value.get_or_init(|| value))
    }
}

/// A pre-compiled component mapped from a file, and its linked form once an
/// executor has been built from it.
#[cfg(any(feature = "embedded", feature = "preinit"))]
pub(crate) struct DeserializedComponent {
    component: Component,
    instance_pre: RetryableOnce<SandboxPre<ExecutorState>>,
}

/// A process-lifetime epoch ticker.
struct EpochTicker {
    handle: std::thread::JoinHandle<()>,
    /// The process that started the thread: a forked child inherits this
    /// handle but not the thread, so its epochs would never advance.
    pid: u32,
}

impl EpochTicker {
    /// Start the ticker thread, advancing every engine in `engines` (which
    /// can grow while it runs) once per tick.
    fn start(engines: Arc<Mutex<Vec<Engine>>>) -> std::result::Result<Self, Error> {
        // Use a dedicated OS thread because guest execution can block the Tokio
        // runtime. Epochs must continue advancing independently of async scheduling.
        let handle = std::thread::Builder::new()
            .name("eryx-epoch-ticker".to_string())
            .spawn(move || {
                loop {
                    std::thread::sleep(EPOCH_TICK_INTERVAL);
                    let engines = engines
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for engine in engines.iter() {
                        engine.increment_epoch();
                    }
                }
            })
            .map_err(|error| {
                ErrorKind::EpochTicker(format!("Failed to start epoch ticker: {error}"))
            })?;

        Ok(Self {
            handle,
            pid: std::process::id(),
        })
    }
}

/// Observe the process-lifetime ticker state.
///
/// A finished worker is removed before returning its error, so the next call
/// can retry startup. There is necessarily a small check-to-execution race: a
/// worker can exit immediately after `is_finished` returns false.
fn observe_epoch_ticker(ticker: &mut Option<EpochTicker>) -> std::result::Result<bool, Error> {
    let Some(dead) = ticker.take() else {
        return Ok(false);
    };

    // Started before a fork: the thread only exists in the parent. Forget the
    // handle (joining or detaching a thread this process doesn't have is
    // unsound) so a fresh ticker is started.
    if dead.pid != std::process::id() {
        std::mem::forget(dead);
        return Ok(false);
    }

    if !dead.handle.is_finished() {
        *ticker = Some(dead);
        return Ok(true);
    }

    let detail = match dead.handle.join() {
        Ok(()) => "epoch ticker worker exited unexpectedly".to_string(),
        Err(payload) => {
            let panic = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            format!("epoch ticker worker panicked: {panic}")
        }
    };
    Err(ErrorKind::EpochTicker(detail).into())
}

pub(crate) fn is_already_cancelled(token: Option<&CancellationToken>) -> bool {
    token.is_some_and(CancellationToken::is_cancelled)
}

/// Start or restart the process-lifetime epoch ticker, and have it advance
/// `engine` from now on.
///
/// One thread ticks every engine it has been given (the shared engine and
/// the child engine), so a store on any of them gets its deadline callbacks,
/// fuel charging and timeouts. A restarted ticker keeps the engines.
pub(crate) fn ensure_epoch_ticker(engine: &Engine) -> std::result::Result<(), Error> {
    // Per process, so a forked child never waits on a lock it inherited, and
    // ticks its own engines.
    static EPOCH_TICKER: ForkLocal<Mutex<Option<EpochTicker>>> =
        ForkLocal::new(|| Mutex::new(None));
    static TICKED_ENGINES: ForkLocal<Arc<Mutex<Vec<Engine>>>> = ForkLocal::new(Arc::default);

    let engines = TICKED_ENGINES.get();
    {
        let mut engines = engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !engines.iter().any(|e| Engine::same(e, engine)) {
            engines.push(engine.clone());
        }
    }

    let mut ticker = EPOCH_TICKER
        .get()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if observe_epoch_ticker(&mut ticker)? {
        return Ok(());
    }

    // Spawn errors are deliberately not cached. A later execution can retry.
    *ticker = Some(EpochTicker::start(Arc::clone(engines))?);
    Ok(())
}

/// Arm a store-local timeout deadline, or an effectively unlimited deadline
/// when no timeout is configured. The deadline remains local to this Store;
/// only the epoch clock itself is shared by executions.
pub(crate) fn arm_epoch_deadline<T>(store: &mut Store<T>, timeout: Option<Duration>) {
    store.set_epoch_deadline(timeout.map_or(u64::MAX / 2, epoch_ticks));
    store.epoch_deadline_trap();
}

/// Arm a store-local deadline that interrupts when `cancel_token` is cancelled
/// (setting `was_cancelled`) or `timeout` elapses.
///
/// Cancellation is checked once per epoch tick. The remaining timeout is kept
/// in this Store's callback so executions share only the clock, not deadline
/// state. The caller must have started the epoch ticker.
pub(crate) fn arm_epoch_cancellation<T: 'static>(
    store: &mut Store<T>,
    timeout: Option<Duration>,
    cancel_token: CancellationToken,
    was_cancelled: Arc<AtomicBool>,
) {
    let mut timeout_ticks = timeout.map(epoch_ticks);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_store| {
        if cancel_token.is_cancelled() {
            was_cancelled.store(true, Ordering::Relaxed);
            return Ok(UpdateDeadline::Interrupt);
        }

        // Yield on non-interrupt ticks so Tokio can run cancellation
        // tasks while CPU-bound guest code is executing. Timeout-only
        // stores use the non-yielding deadline trap path instead.
        match timeout_ticks.as_mut() {
            Some(ticks) if *ticks <= 1 => Ok(UpdateDeadline::Interrupt),
            Some(ticks) => {
                *ticks -= 1;
                Ok(UpdateDeadline::Yield(1))
            }
            None => Ok(UpdateDeadline::Yield(1)),
        }
    });
}

/// Classify an epoch interrupt from the state captured when it was triggered.
///
/// Do not consult the live cancellation token here: it may be cancelled after
/// a timeout has already fired, which would misclassify that timeout.
pub(crate) fn classify_epoch_interrupt(
    was_cancelled_at_interrupt: bool,
    execution_timeout: Option<Duration>,
) -> Error {
    if was_cancelled_at_interrupt {
        ErrorKind::Cancelled.into()
    } else {
        ErrorKind::Timeout(execution_timeout.unwrap_or_default()).into()
    }
}

/// The result a callback handler sends back to the `invoke` host import.
///
/// Distinguishes a successful result and an ordinary error (both surface to
/// Python as a value / exception and let execution continue) from a
/// **suspension**, which causes the import to poison the store's fuel and halt
/// the guest synchronously.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CallbackHostResult {
    /// The callback succeeded; the JSON value is returned to Python.
    Ok(String),
    /// The callback failed; the message is raised as an exception in Python.
    Err(String),
    /// The callback asked to suspend. The import poisons fuel to halt the guest;
    /// the string is the opaque reason recorded for the caller.
    Suspend(String),
}

/// Request to invoke a callback from Python code.
#[derive(Debug)]
#[non_exhaustive]
pub struct CallbackRequest {
    /// Name of the callback to invoke.
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments_json: String,
    /// Channel to send the response back.
    pub response_tx: oneshot::Sender<CallbackHostResult>,
}

impl CallbackRequest {
    /// A request to invoke callback `name` with `arguments_json`, answered on
    /// `response_tx`. Useful for testing a callback handler.
    #[must_use]
    #[cfg(feature = "unstable-internals")]
    pub fn new(
        name: impl Into<String>,
        arguments_json: impl Into<String>,
        response_tx: oneshot::Sender<CallbackHostResult>,
    ) -> Self {
        Self {
            name: name.into(),
            arguments_json: arguments_json.into(),
            response_tx,
        }
    }
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

/// Request to stream output from Python code in real-time.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OutputRequest {
    /// Stream identifier: 0 = stdout, 1 = stderr.
    pub stream: u32,
    /// The raw bytes that were written.
    pub data: Vec<u8>,
}

impl OutputRequest {
    /// A chunk of output `data` on `stream` (0 = stdout, 1 = stderr). Useful
    /// for testing an output handler.
    #[must_use]
    #[cfg(feature = "unstable-internals")]
    pub fn new(stream: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            stream,
            data: data.into(),
        }
    }
}

/// Request for a network operation from Python code.
///
/// Sent from the WASM host imports to the network handler task via an
/// [`mpsc`](tokio::sync::mpsc) channel. Each request that expects a reply
/// carries a oneshot `response_tx` for the handler to send the result back.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetRequest {
    // TCP operations
    /// Connect to a host over TCP.
    TcpConnect {
        /// Target hostname or IP address.
        host: String,
        /// Target port number.
        port: u16,
        /// Guest-requested connect timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the connection handle (or error).
        response_tx: oneshot::Sender<Result<u32, crate::net::TcpError>>,
    },
    /// Read from a TCP connection.
    TcpRead {
        /// Connection handle returned by [`TcpConnect`](Self::TcpConnect).
        handle: u32,
        /// Maximum number of bytes to read.
        len: u32,
        /// Guest-requested I/O timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the read bytes (or error).
        response_tx: oneshot::Sender<Result<Vec<u8>, crate::net::TcpError>>,
    },
    /// Write to a TCP connection.
    TcpWrite {
        /// Connection handle returned by [`TcpConnect`](Self::TcpConnect).
        handle: u32,
        /// Bytes to write.
        data: Vec<u8>,
        /// Guest-requested I/O timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the number of bytes written (or error).
        response_tx: oneshot::Sender<Result<u32, crate::net::TcpError>>,
    },
    /// Close a TCP connection.
    TcpClose {
        /// Connection handle to close.
        handle: u32,
    },

    // TLS operations
    /// Upgrade a TCP connection to TLS.
    TlsUpgrade {
        /// TCP connection handle to upgrade.
        tcp_handle: u32,
        /// SNI hostname for the TLS handshake.
        hostname: String,
        /// Guest-requested handshake timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the TLS connection handle (or error).
        response_tx: oneshot::Sender<Result<u32, crate::net::TlsError>>,
    },
    /// Read from a TLS connection.
    TlsRead {
        /// TLS connection handle returned by [`TlsUpgrade`](Self::TlsUpgrade).
        handle: u32,
        /// Maximum number of bytes to read.
        len: u32,
        /// Guest-requested I/O timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the read bytes (or error).
        response_tx: oneshot::Sender<Result<Vec<u8>, crate::net::TlsError>>,
    },
    /// Write to a TLS connection.
    TlsWrite {
        /// TLS connection handle returned by [`TlsUpgrade`](Self::TlsUpgrade).
        handle: u32,
        /// Bytes to write.
        data: Vec<u8>,
        /// Guest-requested I/O timeout in milliseconds (`0` = host default).
        timeout_ms: u32,
        /// Channel for returning the number of bytes written (or error).
        response_tx: oneshot::Sender<Result<u32, crate::net::TlsError>>,
    },
    /// Close a TLS connection.
    TlsClose {
        /// TLS connection handle to close.
        handle: u32,
    },

    /// Close all active connections.
    ///
    /// Drops every TCP and TLS stream and clears per-connection state while
    /// preserving network configuration, secrets, and monotonic handle
    /// allocation. Intended for application-controlled cleanup between
    /// executions on a reused session.
    #[cfg(feature = "unstable-internals")]
    CloseAll {
        /// Channel for signalling completion.
        response_tx: oneshot::Sender<()>,
    },
}

/// Callback info for introspection (internal type to avoid conflicts with generated code).
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// Captured stdout from the Python execution (raw bytes).
    pub stdout: Vec<u8>,
    /// Captured stderr from the Python execution (raw bytes).
    pub stderr: Vec<u8>,
    /// Peak memory usage in bytes: the instance's largest linear memory
    /// (in a session, so far; see [`crate::ExecuteStats::peak_memory_bytes`]).
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
    /// JSON-serialized value of the script's result variable, or `None` if the
    /// variable was not set. See [`Executor::with_result_variable`].
    pub result: Option<String>,
    /// Why result capture failed (e.g. the value was not JSON-serializable), or
    /// `None` when capture succeeded or no result variable was set.
    pub result_error: Option<String>,
    /// Whether the session lost its Python state before this run: an earlier
    /// call was cancelled or halted mid-run without a snapshot to roll back
    /// to, so the instance was reset. Reported once, by the first successful
    /// run after the reset; always `false` outside sessions.
    pub state_was_reset: bool,
    /// Whether trace events or streamed output chunks were dropped because
    /// the execution reached the per-stream limit (16 MiB of queued events
    /// each). The captured `stdout`/`stderr` here are unaffected.
    pub stream_truncated: bool,
    /// Whether streamed stdout/stderr chunks were dropped because the
    /// execution reached the per-stream limit. Unlike
    /// [`stream_truncated`](Self::stream_truncated) this ignores dropped trace
    /// events. Also reported by [`Error::output_truncated`] when the run fails.
    pub output_truncated: bool,
}

impl ExecutionOutput {
    /// Create a new execution output.
    ///
    /// The captured `result`/`result_error` fields default to `None`; set them on
    /// the returned value when result capture is available.
    #[must_use]
    pub fn new(
        stdout: Vec<u8>,
        stderr: Vec<u8>,
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
            result: None,
            result_error: None,
            state_was_reset: false,
            stream_truncated: false,
            output_truncated: false,
        }
    }

    /// Decode stdout as UTF-8, replacing invalid sequences with U+FFFD.
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Decode stderr as UTF-8, replacing invalid sequences with U+FFFD.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
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
/// use eryx::{Executor, CpuFeatureLevel};
///
/// // Compile for Fly.io (x86-64-v3, no AVX-512)
/// let cwasm = Executor::precompile_with_options(
///     &wasm_bytes,
///     None,  // native target
///     Some(CpuFeatureLevel::X86_64_V3),
/// )?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CpuFeatureLevel {
    /// The target architecture's own baseline, with no optional CPU features.
    ///
    /// Portable to every CPU the target supports, and the only portable choice
    /// on architectures Cranelift has no psABI presets for — notably aarch64,
    /// where the alternative is [`Self::Native`] baking in whichever machine
    /// happened to run the build. On x86-64 this is equivalent to
    /// [`Self::X86_64`].
    ///
    /// The cost is real: on aarch64 it gives up LSE atomics, FP16, dot-product
    /// and I8MM, so hot code is slower. Use it for artifacts you ship to
    /// machines you do not control.
    Baseline,
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
            "baseline" => Some(Self::Baseline),
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
            Self::Baseline => "baseline",
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
                "Unknown CPU feature level '{}'. Valid values: baseline, x86-64, x86-64-v2, x86-64-v3, x86-64-v4, native",
                s
            )
        })
    }
}

/// How wasmtime allocates sandbox instances. See [`SharedEngineConfig::with_allocator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstanceAllocator {
    /// Reuse pre-reserved slots (the default, except on Windows).
    Pooling,
    /// A fresh `mmap` per instance.
    OnDemand,
}

/// Settings for the process-wide shared engine and its warm-instance pool.
///
/// Every sandbox in the process runs on one engine
/// ([`Executor::shared_engine`]) and shares one warm pool, so these are
/// set once for the process with [`configure_shared_engine`], not per
/// sandbox. Each setting left unset falls back to its environment variable,
/// then to the default; the variables and defaults are documented in the
/// performance guide.
///
/// ```
/// let config = eryx::SharedEngineConfig::default()
///     .with_warm_instances(4)
///     .with_pool_instances(256);
/// # let _ = config;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedEngineConfig {
    warm_instances: Option<usize>,
    allocator: Option<InstanceAllocator>,
    pool_instances: Option<u32>,
    pool_keep_resident_mb: Option<usize>,
    pool_pagemap_scan: Option<bool>,
    pool_release_interval_secs: Option<u64>,
}

impl SharedEngineConfig {
    /// Instances to keep ready per runtime (`ERYX_WARM_INSTANCES`, default 1);
    /// `0` disables the warm pool.
    #[must_use]
    pub fn with_warm_instances(mut self, n: usize) -> Self {
        self.warm_instances = Some(n);
        self
    }

    /// The instance allocator (`ERYX_ALLOCATOR`).
    #[must_use]
    pub fn with_allocator(mut self, allocator: InstanceAllocator) -> Self {
        self.allocator = Some(allocator);
        self
    }

    /// Maximum sandbox instances alive at once with the pooling allocator
    /// (`ERYX_POOL_INSTANCES`, default 1000). Must be positive.
    #[must_use]
    pub fn with_pool_instances(mut self, n: u32) -> Self {
        self.pool_instances = Some(n);
        self
    }

    /// How much of each linear memory to keep mapped between uses, in MiB
    /// (`ERYX_POOL_KEEP_RESIDENT_MB`, default 64).
    #[must_use]
    pub fn with_pool_keep_resident_mb(mut self, mib: usize) -> Self {
        self.pool_keep_resident_mb = Some(mib);
        self
    }

    /// Whether to find dirty pages with Linux's `PAGEMAP_SCAN`
    /// (`ERYX_POOL_PAGEMAP_SCAN`, default on).
    #[must_use]
    pub fn with_pool_pagemap_scan(mut self, enabled: bool) -> Self {
        self.pool_pagemap_scan = Some(enabled);
        self
    }

    /// How often to release memory kept resident for unused slots, in seconds
    /// (`ERYX_POOL_RELEASE_INTERVAL_SECS`, default 30); `0` never releases it.
    #[must_use]
    pub fn with_pool_release_interval_secs(mut self, secs: u64) -> Self {
        self.pool_release_interval_secs = Some(secs);
        self
    }

    /// The value for environment variable `name`: the explicit setting if
    /// there is one, otherwise `env`'s.
    ///
    /// Explicit settings go through the same parsing as the variables, so
    /// both are validated identically.
    fn lookup(&self, name: &str, env: impl Fn(&str) -> Option<String>) -> Option<String> {
        let explicit = match name {
            "ERYX_WARM_INSTANCES" => self.warm_instances.map(|n| n.to_string()),
            "ERYX_ALLOCATOR" => self.allocator.map(|a| {
                match a {
                    InstanceAllocator::Pooling => "pooling",
                    InstanceAllocator::OnDemand => "on-demand",
                }
                .to_string()
            }),
            "ERYX_POOL_INSTANCES" => self.pool_instances.map(|n| n.to_string()),
            "ERYX_POOL_KEEP_RESIDENT_MB" => self.pool_keep_resident_mb.map(|n| n.to_string()),
            "ERYX_POOL_PAGEMAP_SCAN" => self.pool_pagemap_scan.map(|b| u8::from(b).to_string()),
            "ERYX_POOL_RELEASE_INTERVAL_SECS" => {
                self.pool_release_interval_secs.map(|n| n.to_string())
            }
            _ => None,
        };
        explicit.or_else(|| env(name))
    }

    pub(crate) fn allocator_settings(&self) -> std::result::Result<AllocatorSettings, Error> {
        AllocatorSettings::from_lookup(|name| self.lookup(name, |name| std::env::var(name).ok()))
    }

    pub(crate) fn warm_instances(&self) -> Option<String> {
        self.lookup("ERYX_WARM_INSTANCES", |name| std::env::var(name).ok())
    }

    /// The configuration the shared engine and warm pool use.
    ///
    /// The first call fixes it: whatever [`configure_shared_engine`] set, or the
    /// defaults, after which `configure_shared_engine` fails.
    pub(crate) fn current() -> &'static Self {
        ENGINE_CONFIG.get_or_init(Self::default)
    }
}

static ENGINE_CONFIG: OnceLock<SharedEngineConfig> = OnceLock::new();

/// Configure the process-wide shared engine and its warm-instance pool.
///
/// Like rayon's `ThreadPoolBuilder::build_global`, this configures a global
/// that is created lazily: call it once, before creating any sandbox. The
/// settings are read when the shared engine or the warm pool is first used
/// (building a sandbox, or calling [`Executor::shared_engine`]), and
/// cannot change after that. Explicit settings take precedence over the
/// `ERYX_*` environment variables, which still apply to anything left unset.
///
/// # Errors
///
/// Returns [`ErrorKind::WasmEngine`] if a setting (explicit or from the
/// environment) is invalid, or if the configuration was already fixed by an
/// earlier call or by the shared engine being created. A rejected call
/// changes nothing.
pub fn configure_shared_engine(config: SharedEngineConfig) -> std::result::Result<(), Error> {
    config.allocator_settings()?;
    ENGINE_CONFIG.set(config).map_err(|_| {
        ErrorKind::WasmEngine(
            "the shared engine configuration is already fixed: configure_shared_engine must \
             be called once, before any sandbox is created"
                .to_string(),
        )
        .into()
    })
}

/// How the shared engine allocates instance resources (linear memories,
/// tables, and async stacks).
///
/// Set by [`SharedEngineConfig`], falling back to the environment, because the
/// engine is process-global ([`Executor::shared_engine`]) and is
/// created before any sandbox configuration exists.
///
/// - `ERYX_ALLOCATOR`: `pooling` (default, except on Windows) or `on-demand`.
/// - `ERYX_POOL_INSTANCES`: maximum concurrently live sandbox instances with
///   the pooling allocator (default 1000). Every running `Sandbox::execute()`,
///   every live session and every warm instance holds one; instantiation
///   fails once the pool is full.
/// - `ERYX_POOL_KEEP_RESIDENT_MB`: how much of each linear memory to keep
///   mapped between uses (default 64). Dirty pages within this budget are
///   restored with `memcpy` instead of being unmapped, so the next instance
///   in the slot takes no page faults for them.
/// - `ERYX_POOL_PAGEMAP_SCAN`: `1` (default) lets wasmtime find the dirty
///   pages with Linux's `PAGEMAP_SCAN` ioctl (6.7+) and reset only those;
///   `0` resets the whole keep-resident budget with `memcpy` instead, which
///   trades a larger copy for fewer page faults on the next execution.
/// - `ERYX_POOL_RELEASE_INTERVAL_SECS`: every this many seconds (default 30),
///   release the memory kept resident for unused slots back to the OS. `0`
///   keeps it resident indefinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocatorSettings {
    pooling: bool,
    pool_instances: u32,
    keep_resident_bytes: usize,
    pagemap_scan: bool,
    release_interval: Option<Duration>,
}

impl Default for AllocatorSettings {
    fn default() -> Self {
        Self {
            // Windows commits the whole table pool up front (it has no
            // overcommit), which at the default limits is gigabytes of commit
            // charge per process.
            pooling: !cfg!(windows),
            pool_instances: 1000,
            keep_resident_bytes: 64 << 20,
            pagemap_scan: true,
            release_interval: Some(Duration::from_secs(30)),
        }
    }
}

impl AllocatorSettings {
    /// Upper bound on funcref table growth per instance.
    ///
    /// The runtime's function table starts at ~6k entries and grows when
    /// native extensions are `dlopen`ed, so this sits well above wasmtime's
    /// 20k default while still bounding the per-slot address reservation.
    const TABLE_ELEMENTS: usize = 1 << 20;

    /// Core instances one runtime instance needs: 13 for the runtime's own
    /// modules, plus one per native extension `.so`.
    ///
    /// The pool only counts core instances, it reserves nothing per slot, so
    /// generous headroom here costs nothing.
    const CORE_INSTANCES_PER_SANDBOX: u32 = 64;

    /// Tables one runtime instance defines (measured; memories are one
    /// each). Each table slot reserves address space, so this is exact.
    const TABLES_PER_SANDBOX: u32 = 2;

    /// Fiber stacks one runtime instance may hold at once.
    ///
    /// Each guest task the component-model-async runtime has in flight gets
    /// its own fiber, and Python's `asyncio` timers and `gather` keep several
    /// alive (the test suite peaks at 4). A stack slot is address space plus
    /// pages decommitted on release, so headroom is cheap.
    const STACKS_PER_SANDBOX: u32 = 16;

    /// Upper bound on one core instance's `VMContext` metadata.
    ///
    /// The largest with numpy linked in needs 242,088 bytes (measured), but
    /// child commands are bigger: Go's `gh` (wasip3) needs 1,276,940, almost
    /// all of it per-function metadata, past wasmtime's 1 MiB default. 4 MiB
    /// is about 3x that. Like the component limit below, this is only
    /// checked at instantiation: the metadata is heap-allocated at its actual
    /// size, never reserved per slot.
    const CORE_INSTANCE_SIZE: usize = 4 << 20;

    /// Upper bound on one runtime instance's metadata: its
    /// `VMComponentContext` plus every core instance's `VMContext`.
    ///
    /// Each native extension adds a core instance, so this grows with the
    /// extensions linked in. numpy needs 1,170,088 bytes (measured), past
    /// wasmtime's 1 MiB default, which failed every numpy instantiation.
    /// 64 MiB is over 50x that; nothing is reserved, so the headroom costs
    /// nothing.
    const COMPONENT_INSTANCE_SIZE: usize = 64 << 20;

    /// Parse the settings from `lookup`, a view of the environment.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> std::result::Result<Self, Error> {
        let mut settings = Self::default();

        match lookup("ERYX_ALLOCATOR") {
            Some(v) if v.eq_ignore_ascii_case("pooling") => settings.pooling = true,
            Some(v) if v.eq_ignore_ascii_case("on-demand") => settings.pooling = false,
            Some(v) => {
                return Err(ErrorKind::WasmEngine(format!(
                    "Unknown allocator '{v}' in ERYX_ALLOCATOR. Valid values: pooling, on-demand"
                ))
                .into());
            }
            None => {}
        }

        if let Some(v) = lookup("ERYX_POOL_INSTANCES") {
            settings.pool_instances =
                v.trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        ErrorKind::WasmEngine(format!(
                            "ERYX_POOL_INSTANCES must be a positive integer, got '{v}'"
                        ))
                    })?;
        }

        if let Some(v) = lookup("ERYX_POOL_KEEP_RESIDENT_MB") {
            let mib = v.trim().parse::<usize>().map_err(|_| {
                ErrorKind::WasmEngine(format!(
                    "ERYX_POOL_KEEP_RESIDENT_MB must be a non-negative integer, got '{v}'"
                ))
            })?;
            settings.keep_resident_bytes = mib.checked_mul(1 << 20).ok_or_else(|| {
                ErrorKind::WasmEngine(format!(
                    "ERYX_POOL_KEEP_RESIDENT_MB is too large, got '{v}'"
                ))
            })?;
        }

        match lookup("ERYX_POOL_PAGEMAP_SCAN") {
            Some(v) if v.trim() == "0" => settings.pagemap_scan = false,
            Some(v) if v.trim() == "1" => settings.pagemap_scan = true,
            Some(v) => {
                return Err(ErrorKind::WasmEngine(format!(
                    "ERYX_POOL_PAGEMAP_SCAN must be 0 or 1, got '{v}'"
                ))
                .into());
            }
            None => {}
        }

        if let Some(v) = lookup("ERYX_POOL_RELEASE_INTERVAL_SECS") {
            let secs = v.trim().parse::<u64>().map_err(|_| {
                ErrorKind::WasmEngine(format!(
                    "ERYX_POOL_RELEASE_INTERVAL_SECS must be a non-negative integer, got '{v}'"
                ))
            })?;
            settings.release_interval = (secs > 0).then(|| Duration::from_secs(secs));
        }

        Ok(settings)
    }

    /// The pooling configuration to use, or `None` for on-demand allocation.
    pub(crate) fn pooling_config(&self) -> Option<PoolingAllocationConfig> {
        if !self.pooling {
            return None;
        }

        let mut pooling = PoolingAllocationConfig::new();
        pooling
            .total_component_instances(self.pool_instances)
            .total_core_instances(
                self.pool_instances
                    .saturating_mul(Self::CORE_INSTANCES_PER_SANDBOX),
            )
            .total_memories(self.pool_instances)
            .total_tables(self.pool_instances.saturating_mul(Self::TABLES_PER_SANDBOX))
            .total_stacks(self.pool_instances.saturating_mul(Self::STACKS_PER_SANDBOX))
            .max_component_instance_size(Self::COMPONENT_INSTANCE_SIZE)
            .max_core_instance_size(Self::CORE_INSTANCE_SIZE)
            .table_elements(Self::TABLE_ELEMENTS)
            .linear_memory_keep_resident(self.keep_resident_bytes)
            .table_keep_resident(Self::TABLE_ELEMENTS * std::mem::size_of::<usize>())
            .pagemap_scan(if self.pagemap_scan {
                Enabled::Auto
            } else {
                Enabled::No
            });
        Some(pooling)
    }
}

/// Every `interval`, release the pooling allocator's resident memory for
/// unused slots.
///
/// `linear_memory_keep_resident` keeps each slot's pages mapped after use so
/// the next instance in it takes no page faults. That is the point under
/// load, but after a burst of concurrent executions it leaves every slot the
/// burst touched resident indefinitely. Live instances, including warm ones,
/// are untouched; a slot released while traffic continues costs one slower
/// instantiation per interval.
///
/// Does nothing for an engine without the pooling allocator.
fn start_pool_release(engine: &Engine, interval: Duration) {
    if engine.pooling_allocator_metrics().is_none() {
        return;
    }
    let engine = engine.clone();
    let spawned = std::thread::Builder::new()
        .name("eryx-pool-release".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                let released = engine.release_idle_pool_memory();
                if released > 0 {
                    tracing::debug!(released, "released unused pooling allocator memory");
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "failed to start the pool release thread");
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
    /// For a guest command (a child), its share of the children's memory
    /// pool, which its memory and tables are charged to.
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    child: Option<crate::process::ChildMemory>,
}

impl MemoryTracker {
    /// Create a new memory tracker with an optional limit.
    #[must_use]
    pub fn new(memory_limit: Option<u64>) -> Self {
        Self {
            peak_memory_bytes: AtomicU64::new(0),
            memory_limit,
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            child: None,
        }
    }

    /// A tracker for a guest command, charging `child`, its share of the
    /// children's memory pool.
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    pub(crate) fn for_child(child: crate::process::ChildMemory) -> Self {
        Self {
            child: Some(child),
            ..Self::new(None)
        }
    }

    /// Get the peak memory usage observed so far (in bytes).
    #[must_use]
    pub fn peak_memory_bytes(&self) -> u64 {
        self.peak_memory_bytes.load(Ordering::Relaxed)
    }

    /// Record memory that was already in use before this tracker was installed.
    pub(crate) fn observe(&self, bytes: u64) {
        self.peak_memory_bytes.fetch_max(bytes, Ordering::Relaxed);
    }
}

impl ResourceLimiter for MemoryTracker {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
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

        // A guest command's growth is charged to the children's pool last,
        // once nothing else refuses it.
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        if let Some(child) = &mut self.child {
            return child.memory_growing(current, desired, maximum);
        }
        #[cfg(not(any(feature = "unstable-vfs", feature = "shell")))]
        let _ = current;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // Allow table growth up to the declared maximum
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        if let Some(child) = &mut self.child {
            return child.table_growing(current, desired, maximum);
        }
        #[cfg(not(any(feature = "unstable-vfs", feature = "shell")))]
        let _ = current;
        Ok(true)
    }
}

// Generate bindings from the WIT file
//
// Network functions are declared as regular sync `func` in WIT (not `async func`).
// We use the `async` flag for network imports to generate `func_wrap_async` bindings.
// This gives us fiber-based async: the host can await on async operations, but from
// the guest's perspective the calls are blocking. This requires
// `Config::wasm_component_model_async`.
wasmtime::component::bindgen!({
    path: "wit",
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

/// Most bytes kept per stream per execution from direct fd 1/2 writes. These
/// are streamed once at the end of the execution, on top of
/// [`STREAM_BYTES_LIMIT`].
// ponytail: fixed cap, excess dropped; make it a ResourceLimits field if anyone needs more.
const FD_CAPTURE_LIMIT: usize = 1024 * 1024;

/// Guest stdout/stderr at the WASI level (fds 1 and 2).
///
/// Python's `sys.stdout`/`sys.stderr` are captured inside the guest, but
/// `os.write(1, ...)` and `sys.__stdout__` go straight to the WASI streams.
/// This collects those bytes instead of passing them to the host's stdio, so
/// they can be appended to the execution's captured output. At most
/// [`FD_CAPTURE_LIMIT`] bytes are kept between [`take`](Self::take)s; the
/// rest is dropped.
#[derive(Clone, Default)]
pub(crate) struct FdCapture(Arc<Mutex<Vec<u8>>>);

impl FdCapture {
    /// Remove and return everything captured so far.
    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn push(&self, data: &[u8]) {
        let mut buf = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let room = FD_CAPTURE_LIMIT.saturating_sub(buf.len());
        buf.extend_from_slice(&data[..data.len().min(room)]);
    }
}

impl wasmtime_wasi::cli::IsTerminal for FdCapture {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl wasmtime_wasi::cli::StdoutStream for FdCapture {
    fn p2_stream(&self) -> Box<dyn wasmtime_wasi_io::streams::OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl wasmtime_wasi_io::streams::OutputStream for FdCapture {
    fn write(
        &mut self,
        bytes: wasmtime_wasi_io::bytes::Bytes,
    ) -> Result<(), wasmtime_wasi_io::streams::StreamError> {
        self.push(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), wasmtime_wasi_io::streams::StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, wasmtime_wasi_io::streams::StreamError> {
        Ok(64 * 1024)
    }
}

#[async_trait::async_trait]
impl wasmtime_wasi_io::poll::Pollable for FdCapture {
    async fn ready(&mut self) {}
}

impl tokio::io::AsyncWrite for FdCapture {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.push(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Captures of a guest's fd 1 and fd 2.
#[derive(Clone, Default)]
pub(crate) struct FdOutput {
    pub(crate) stdout: FdCapture,
    pub(crate) stderr: FdCapture,
}

impl FdOutput {
    /// Route the guest's fds 1 and 2 into these captures.
    pub(crate) fn attach(&self, wasi_builder: &mut WasiCtxBuilder) {
        wasi_builder
            .stdout(self.stdout.clone())
            .stderr(self.stderr.clone());
    }

    /// Take what was captured since the last call, as `(stdout, stderr)`,
    /// also sending it to the output stream if one is attached.
    pub(crate) fn take(
        &self,
        output_tx: Option<&mpsc::UnboundedSender<OutputRequest>>,
    ) -> (Vec<u8>, Vec<u8>) {
        let streams = (self.stdout.take(), self.stderr.take());
        if let Some(tx) = output_tx {
            for (stream, data) in [(0, &streams.0), (1, &streams.1)] {
                if !data.is_empty() {
                    let _ = tx.send(OutputRequest {
                        stream,
                        data: data.clone(),
                    });
                }
            }
        }
        streams
    }
}

/// Most bytes one execution may queue for each of the trace and output
/// streams, each queued event or chunk also being charged a small fixed
/// overhead. The imports that feed them can't wait for a slow consumer, so
/// anything past this is dropped from the stream (captured stdout/stderr in
/// the final result are unaffected) and the execution reports
/// [`ExecutionOutput::stream_truncated`]. A consumer that builds its output
/// from the stream, like `eryx-server`, can't keep more than this.
// ponytail: fixed per-execution cap; make it configurable if a consumer needs more.
pub const STREAM_BYTES_LIMIT: usize = 16 * 1024 * 1024;

/// What each queued trace event or output chunk is charged on top of its
/// payload, for its allocation and channel slot. With [`STREAM_BYTES_LIMIT`]
/// this also caps the number of queued messages per stream.
const STREAM_MESSAGE_OVERHEAD: usize = 64;

/// What is left of this execution's trace and output stream budgets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamBudget {
    trace: usize,
    output: usize,
    /// Whether anything was dropped for lack of budget.
    pub(crate) truncated: bool,
    /// Whether output chunks (not trace events) were dropped.
    pub(crate) output_truncated: bool,
}

impl Default for StreamBudget {
    fn default() -> Self {
        Self {
            trace: STREAM_BYTES_LIMIT,
            output: STREAM_BYTES_LIMIT,
            truncated: false,
            output_truncated: false,
        }
    }
}

impl StreamBudget {
    /// Charge one trace event with `payload` bytes; `false` means drop it.
    fn spend_trace(&mut self, payload: usize) -> bool {
        Self::spend(&mut self.trace, &mut self.truncated, payload, "trace")
    }

    /// Charge one output chunk with `payload` bytes; `false` means drop it.
    fn spend_output(&mut self, payload: usize) -> bool {
        let spent = Self::spend(&mut self.output, &mut self.truncated, payload, "output");
        self.output_truncated |= !spent;
        spent
    }

    fn spend(left: &mut usize, truncated: &mut bool, payload: usize, stream: &str) -> bool {
        let cost = payload.saturating_add(STREAM_MESSAGE_OVERHEAD);
        if let Some(rest) = left.checked_sub(cost) {
            *left = rest;
            return true;
        }
        if *left != 0 {
            *left = 0;
            tracing::warn!(
                stream,
                limit = STREAM_BYTES_LIMIT,
                "stream limit reached; dropping further events"
            );
        }
        *truncated = true;
        false
    }
}

/// Most callback starts [`TraceClock`] keeps waiting for their end; past
/// it, the oldest is forgotten (its end then reports no duration).
const MAX_PENDING_CALLBACKS: usize = 1024;

/// The host's part in an execution's trace events: timing callbacks, and
/// numbering child processes.
#[derive(Debug, Default)]
pub(crate) struct TraceClock {
    /// Callbacks started and not yet ended, oldest first, by name.
    ///
    /// The guests report `callback_start` and `callback_end` around a call
    /// but don't time it. Calls are matched by name, first in first out. Only
    /// events the trace budget let through are timed, and at most
    /// [`MAX_PENDING_CALLBACKS`] starts are kept, so a guest reporting starts
    /// without ends can't grow it.
    // ponytail: concurrent calls to one callback that finish out of order get
    // each other's durations; a call id in the events would pair them exactly.
    // A start whose end never comes skews that name's later durations the
    // same way: the CPython guest reports no end for a call abandoned at the
    // end of an execution (a task still pending), since a garbage collection
    // closes it in a later execution, where an end would be paired with
    // another call.
    callbacks: VecDeque<(String, std::time::Instant)>,
    /// The last child process's pid.
    last_pid: u32,
    /// Children started and not yet ended.
    running: Vec<RunningChild>,
}

impl TraceClock {
    /// Note a `callback_start`, or add `duration_ms` to a `callback_end`.
    fn time(&mut self, event_json: &mut String) {
        // Only parse the events that can be callback events.
        if !event_json.contains("callback_") {
            return;
        }
        let Ok(serde_json::Value::Object(mut event)) = serde_json::from_str(event_json) else {
            return;
        };
        let (Some(kind), Some(name)) = (
            event.get("type").and_then(serde_json::Value::as_str),
            event.get("name").and_then(serde_json::Value::as_str),
        ) else {
            return;
        };
        match kind {
            "callback_start" => {
                if self.callbacks.len() == MAX_PENDING_CALLBACKS {
                    self.callbacks.pop_front();
                }
                self.callbacks
                    .push_back((name.to_owned(), std::time::Instant::now()));
            }
            "callback_end" => {
                let Some(i) = self
                    .callbacks
                    .iter()
                    .position(|(started, _)| started == name)
                else {
                    return;
                };
                let Some((_, started)) = self.callbacks.remove(i) else {
                    return;
                };
                let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                event.insert("duration_ms".to_owned(), ms.into());
                *event_json = serde_json::Value::Object(event).to_string();
            }
            _ => {}
        }
    }
}

/// A child process whose `process_start` was reported and whose
/// `process_end` wasn't yet.
#[derive(Debug)]
struct RunningChild {
    /// Identifies the child across executions; see [`ProcessTrace`].
    key: u64,
    name: String,
    pid: u32,
    started: std::time::Instant,
}

impl RunningChild {
    /// Its `process_end` event: it exited with a code, or failed.
    fn end_event(&self, outcome: Result<i32, &str>, ended: std::time::Instant) -> String {
        let duration = ended.saturating_duration_since(self.started);
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let mut event = serde_json::json!({
            "type": "process_end",
            "name": self.name,
            "pid": self.pid,
            "duration_ms": duration_ms,
        });
        match outcome {
            Ok(code) => event["exit_code"] = code.into(),
            Err(error) => event["error"] = error.into(),
        }
        event.to_string()
    }
}

/// A child process's trace, from [`ExecutorState::trace_process_start`] to
/// [`Self::end`].
///
/// It holds no sender: a session keeps a `wait` its timeout interrupted
/// parked in the store, and a sender held there would keep the trace channel
/// open, so the execution would never finish. A child still running when the
/// execution's trace channel goes is reported killed then instead
/// ([`ExecutorState::end_running_children`]).
#[must_use]
pub(crate) struct ProcessTrace {
    key: u64,
}

impl ProcessTrace {
    /// Report that the child exited with `code`, or failed with `error`,
    /// unless its execution already reported it killed.
    pub(crate) fn end(self, state: &mut ExecutorState, outcome: Result<i32, &str>) {
        self.end_at(state, outcome, std::time::Instant::now());
    }

    /// [`end`](Self::end), for a child that ended at `ended` (a nested one,
    /// reported later).
    pub(crate) fn end_at(
        self,
        state: &mut ExecutorState,
        outcome: Result<i32, &str>,
        ended: std::time::Instant,
    ) {
        let running = &mut state.trace_clock.running;
        if let Some(i) = running.iter().position(|child| child.key == self.key) {
            let child = running.swap_remove(i);
            state.send_trace(0, child.end_event(outcome, ended), String::new());
        }
    }
}

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
    /// Channel to stream output (stdout/stderr) to the host in real-time.
    pub(crate) output_tx: Option<mpsc::UnboundedSender<OutputRequest>>,
    /// Hybrid virtual filesystem context (with `unstable-vfs`, or `shell`
    /// for the shell's own filesystem).
    /// Routes /data/* to VFS storage, other paths to real filesystem.
    /// Uses ScrubbingStorage to scrub secret placeholders from file writes.
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    pub(crate) hybrid_vfs_ctx: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
    /// Set when a callback requested suspension (`CallbackError::Suspend`).
    ///
    /// The callback import sets this synchronously and poisons the store's fuel
    /// to `0`, which traps the guest at its next fuel-metered instruction. The
    /// flag both gates further host effects (network / VFS / output) and lets the
    /// trap be classified as a suspension rather than a fuel-limit exhaustion.
    pub(crate) suspended: Option<String>,
    /// Whether this execution uses a fresh instance whose initialized empty
    /// callback state can be reused safely.
    ///
    /// Reported to the guest as `reuse-empty-callbacks`. Current guests ignore
    /// it and reinstall callbacks only when the declarations change; older
    /// guests use it to skip setup on fresh, callback-free instances.
    pub(crate) reuse_empty_callbacks: bool,
    /// Remaining trace/output stream bytes for the current execution.
    pub(crate) stream_budget: StreamBudget,
    /// External commands the guest can spawn through `eryx:process`.
    pub(crate) commands: crate::process::Commands,
    /// Host memory the execution's child processes may hold.
    pub(crate) child_budget: crate::process::ChildBudget,
    /// The execution's children's network, if they have one, shared with
    /// children that spawn their own. Dropped (with its proxies) when the
    /// execution ends.
    pub(crate) child_net: Option<std::sync::Arc<std::sync::Mutex<crate::process::ChildNet>>>,
    /// When each callback still running started, for `callback_end`'s
    /// duration.
    pub(crate) trace_clock: TraceClock,
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
            .field("net_tx", &self.net_tx.is_some())
            .field("output_tx", &self.output_tx.is_some());
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        debug.field("hybrid_vfs_ctx", &self.hybrid_vfs_ctx.is_some());
        debug.finish()
    }
}

impl Drop for ExecutorState {
    fn drop(&mut self) {
        // A dropped store (a timed-out or cancelled one-shot execution) drops
        // its children's `wait`s; report them before the trace channel goes.
        self.end_running_children();
    }
}

impl ExecutorState {
    /// State for instantiating a store ahead of any execution.
    ///
    /// Instantiation runs no guest code beyond table initialisation, so the
    /// guest never observes this state; [`Executor::execute_internal`]
    /// replaces it wholesale with the real per-execution state before the
    /// first export call.
    pub(crate) fn placeholder() -> Self {
        Self {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            callback_tx: None,
            trace_tx: None,
            callbacks: Vec::new(),
            memory_tracker: MemoryTracker::new(None),
            net_tx: None,
            output_tx: None,
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            hybrid_vfs_ctx: None,
            suspended: None,
            reuse_empty_callbacks: true,
            stream_budget: StreamBudget::default(),
            commands: crate::process::Commands::default(),
            child_budget: crate::process::ChildBudget::default(),
            child_net: None,
            trace_clock: TraceClock::default(),
        }
    }

    /// Send a trace event, if tracing is on and the stream budget allows;
    /// whether it was sent.
    fn send_trace(&mut self, lineno: u32, event_json: String, context_json: String) -> bool {
        let charged = self.charge_trace(&event_json, &context_json);
        if charged {
            self.send_charged_trace(lineno, event_json, context_json);
        }
        charged
    }

    /// Whether tracing is on and the stream budget has room for an event;
    /// charges it if so.
    fn charge_trace(&mut self, event_json: &str, context_json: &str) -> bool {
        self.trace_tx.is_some()
            && self
                .stream_budget
                .spend_trace(event_json.len() + context_json.len())
    }

    /// Send an event without charging it: [`Self::charge_trace`] did, or
    /// it's exempt.
    fn send_charged_trace(&self, lineno: u32, event_json: String, context_json: String) {
        if let Some(tx) = &self.trace_tx {
            // Fire-and-forget - trace events are not critical
            let _ = tx.send(TraceRequest {
                lineno,
                event_json,
                context_json,
            });
        }
    }

    /// Report that a child process `argv` started, numbering it; `None` if
    /// tracing is off or the start wasn't sent, so no end is reported either.
    pub(crate) fn trace_process_start(&mut self, argv: &[String]) -> Option<ProcessTrace> {
        self.trace_process_start_at(argv, std::time::Instant::now())
    }

    /// [`trace_process_start`](Self::trace_process_start), for a child that
    /// started at `started` (a nested one, reported later).
    pub(crate) fn trace_process_start_at(
        &mut self,
        argv: &[String],
        started: std::time::Instant,
    ) -> Option<ProcessTrace> {
        /// Tells apart children of different executions of one session, whose
        /// pids both start at 1.
        static KEYS: AtomicU64 = AtomicU64::new(0);
        let (name, args) = argv.split_first()?;
        let pid = self.trace_clock.last_pid + 1;
        let event = serde_json::json!({ "type": "process_start", "name": name, "pid": pid });
        let context = serde_json::json!({ "args": args });
        if !self.send_trace(0, event.to_string(), context.to_string()) {
            return None;
        }
        self.trace_clock.last_pid = pid;
        let key = KEYS.fetch_add(1, Ordering::Relaxed);
        self.trace_clock.running.push(RunningChild {
            key,
            name: name.clone(),
            pid,
            started,
        });
        Some(ProcessTrace { key })
    }

    /// Report every child still running as killed: its execution is ending
    /// (or timed out, or was cancelled) with the child's `wait` unfinished.
    ///
    /// Uncharged, so a timeout that comes once the budget is spent still
    /// pairs each start; at most one event per running child.
    pub(crate) fn end_running_children(&mut self) {
        for child in std::mem::take(&mut self.trace_clock.running) {
            let event = child.end_event(Err("killed"), std::time::Instant::now());
            self.send_charged_trace(0, event, String::new());
        }
    }

    /// Drop every channel to the host so their receivers see end-of-stream,
    /// and stop the children's proxies.
    pub(crate) fn disconnect(&mut self) {
        self.end_running_children();
        self.callback_tx = None;
        self.trace_tx = None;
        self.net_tx = None;
        self.output_tx = None;
        self.child_net = None;
    }
}

/// Create a store around `state` and instantiate `pre` into it, leaving
/// `initial_fuel` in the tank for user code.
///
/// The epoch deadline is left effectively unbounded so instantiation cannot be
/// interrupted; callers arm the real deadline before running user code.
/// Instantiation burns fuel of its own (the modules' table-initialising start
/// functions), so it runs on an unlimited tank and `initial_fuel` is set
/// afterwards: `fuel_consumed` then measures only the user's code and is the
/// same whether or not the store came from the warm pool.
pub(crate) async fn instantiate_store(
    pre: &SandboxPre<ExecutorState>,
    state: ExecutorState,
    initial_fuel: u64,
) -> std::result::Result<(Store<ExecutorState>, Sandbox), Error> {
    let set_fuel = |store: &mut Store<ExecutorState>, fuel: u64| {
        store
            .set_fuel(fuel)
            .map_err(|e| ErrorKind::Initialization(format!("Failed to set fuel: {e}")))
    };

    let mut store = Store::new(pre.engine(), state);
    store.limiter(|state| &mut state.memory_tracker);
    store.set_epoch_deadline(u64::MAX / 2);
    set_fuel(&mut store, u64::MAX)?;
    sync_fuel_periodically(&mut store)?;

    let bindings = pre
        .instantiate_async(&mut store)
        .await
        .map_err(ErrorKind::wasm_component)?;

    set_fuel(&mut store, initial_fuel)?;
    Ok((store, bindings))
}

impl ExecutorState {
    /// State for running the guest as a command (see
    /// [`Executor::run_command`]): its stdio is `wasi`'s and its filesystem
    /// `vfs`, and it gets no callbacks, tracing, network or commands.
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    pub(crate) fn for_command(
        wasi: WasiCtx,
        vfs: eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>,
        memory_tracker: MemoryTracker,
    ) -> Self {
        Self {
            wasi,
            // A shell's child, capped like other children's (see
            // `process::child_table`).
            table: crate::process::child_table(),
            callback_tx: None,
            trace_tx: None,
            callbacks: Vec::new(),
            memory_tracker,
            net_tx: None,
            output_tx: None,
            hybrid_vfs_ctx: Some(vfs),
            suspended: None,
            reuse_empty_callbacks: true,
            stream_budget: StreamBudget::default(),
            commands: crate::process::Commands::default(),
            child_budget: crate::process::ChildBudget::default(),
            child_net: None,
            trace_clock: TraceClock::default(),
        }
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

#[cfg(any(feature = "unstable-vfs", feature = "shell"))]
impl eryx_vfs::HybridVfsView for ExecutorState {
    type Storage = eryx_vfs::ArcStorage;

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
// wasmtime 47 moved the store type parameter off the individual methods and
// onto the `*WithStore` trait itself; the accessor argument is unchanged.
impl<T> SandboxImportsWithStore<T> for HasSelf<ExecutorState> {
    /// Invoke a callback by name with JSON arguments (async).
    fn invoke(
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

                // Send request to the callback handler, then release our sender
                // clone before awaiting the response. If the guest returns with
                // this call still in flight, a session's long-lived store keeps
                // this future parked; a sender held here would keep the channel
                // open and the caller's wait on the handler would never end.
                let sent = tx.send(request).await;
                drop(tx);
                if sent.is_err() {
                    Err("Callback channel closed".to_string())
                } else {
                    // Wait for the handler's response.
                    match response_rx.await {
                        Ok(CallbackHostResult::Ok(value)) => Ok(value),
                        Ok(CallbackHostResult::Err(message)) => Err(message),
                        Ok(CallbackHostResult::Suspend(reason)) => {
                            // Suspension is a synchronous callback-boundary event:
                            // poison the store's fuel to 0 so the guest traps at
                            // its next fuel-metered instruction — Python cannot run
                            // any further code, dispatch callbacks, or perform I/O
                            // afterwards. The host-side flag records the reason and
                            // drives trap classification plus the net/VFS/output
                            // gates.
                            accessor.with(|mut access| {
                                access.get().suspended = Some(reason.clone());
                                let _ = access.as_context_mut().set_fuel(0);
                            });
                            // The return value is moot: the guest traps before it
                            // can observe it. Surface the reason for completeness.
                            Err(reason)
                        }
                        Err(_) => Err("Callback response channel closed".to_string()),
                    }
                }
            } else {
                // No callback channel - return error
                Err(format!("Callback '{name}' not available (no handler)"))
            }
        }
    }

    /// Back an event-loop timer. The guest is suspended meanwhile, so it burns
    /// no fuel; the execution timeout still applies because this future runs
    /// inside the timed `run_concurrent`, and is dropped with it.
    fn sleep(
        _accessor: &Accessor<T, Self>,
        duration_ns: u64,
    ) -> impl ::core::future::Future<Output = ()> + Send {
        tokio::time::sleep(Duration::from_nanos(duration_ns))
    }
}

impl SandboxImports for ExecutorState {
    /// List all available callbacks for introspection.
    ///
    /// Sorted by name: the guest compares this list, serialized, with the set
    /// whose wrappers it has installed (possibly baked into the pre-init
    /// snapshot from `PreInitOptions::callbacks`, which sorts the same way), so
    /// registration order must not affect whether the sets match.
    fn list_callbacks(&mut self) -> Vec<CallbackInfo> {
        let mut infos: Vec<CallbackInfo> = self
            .callbacks
            .iter()
            .map(|cb| CallbackInfo {
                name: cb.name.clone(),
                description: cb.description.clone(),
                parameters_schema_json: cb.parameters_schema_json.clone(),
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Report a trace event to the host.
    fn report_trace(&mut self, lineno: u32, mut event_json: String, context_json: String) {
        if self.charge_trace(&event_json, &context_json) {
            // Only once charged: the clock keeps what a start's event names.
            // (An end's added `duration_ms` goes uncharged, a few bytes.)
            self.trace_clock.time(&mut event_json);
            self.send_charged_trace(lineno, event_json, context_json);
        }
    }

    /// Report output (stdout/stderr) to the host in real-time.
    fn report_output(&mut self, stream_id: u32, data: Vec<u8>) {
        // Drop output once suspended: no side effects after a suspension.
        if self.suspended.is_some() {
            return;
        }
        if let Some(tx) = &self.output_tx {
            if data.is_empty() || !self.stream_budget.spend_output(data.len()) {
                return;
            }
            let request = OutputRequest {
                stream: stream_id,
                data,
            };
            // Fire-and-forget - output streaming is not critical
            let _ = tx.send(request);
        }
    }

    /// Read host-controlled behavior for this execution.
    fn get_execution_options(&mut self) -> ExecutionOptions {
        ExecutionOptions {
            tracing: self.trace_tx.is_some(),
            reuse_empty_callbacks: self.reuse_empty_callbacks,
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
    async fn connect(
        &mut self,
        host: String,
        port: u16,
        timeout_ms: u32,
    ) -> Result<u32, tcp::TcpError> {
        tracing::debug!(host = %host, port, timeout_ms, "TCP connect requested");

        if self.suspended.is_some() {
            return Err(tcp::TcpError::NotPermitted("execution suspended".into()));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpConnect {
            host,
            port,
            timeout_ms,
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

    async fn read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, tcp::TcpError> {
        tracing::trace!(handle, len, timeout_ms, "TCP read requested");

        if self.suspended.is_some() {
            return Err(tcp::TcpError::NotPermitted("execution suspended".into()));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpRead {
            handle,
            len,
            timeout_ms,
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

    async fn write(
        &mut self,
        handle: u32,
        timeout_ms: u32,
        data: Vec<u8>,
    ) -> Result<u32, tcp::TcpError> {
        tracing::trace!(handle, len = data.len(), timeout_ms, "TCP write requested");

        if self.suspended.is_some() {
            return Err(tcp::TcpError::NotPermitted("execution suspended".into()));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tcp::TcpError::NotPermitted("networking not enabled for this sandbox".into())
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TcpWrite {
            handle,
            data,
            timeout_ms,
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
    async fn upgrade(
        &mut self,
        tcp_handle: u32,
        hostname: String,
        timeout_ms: u32,
    ) -> Result<u32, tls::TlsError> {
        tracing::debug!(tcp_handle, hostname = %hostname, timeout_ms, "TLS upgrade requested");

        if self.suspended.is_some() {
            return Err(tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "execution suspended".into(),
            )));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsUpgrade {
            tcp_handle,
            hostname,
            timeout_ms,
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

    async fn read(
        &mut self,
        handle: u32,
        len: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, tls::TlsError> {
        tracing::trace!(handle, len, timeout_ms, "TLS read requested");

        if self.suspended.is_some() {
            return Err(tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "execution suspended".into(),
            )));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsRead {
            handle,
            len,
            timeout_ms,
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

    async fn write(
        &mut self,
        handle: u32,
        timeout_ms: u32,
        data: Vec<u8>,
    ) -> Result<u32, tls::TlsError> {
        tracing::trace!(handle, len = data.len(), timeout_ms, "TLS write requested");

        if self.suspended.is_some() {
            return Err(tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "execution suspended".into(),
            )));
        }

        let tx = self.net_tx.clone().ok_or_else(|| {
            tls::TlsError::Tcp(tcp::TcpError::NotPermitted(
                "networking not enabled for this sandbox".into(),
            ))
        })?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = NetRequest::TlsWrite {
            handle,
            data,
            timeout_ms,
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
/// Created by [`Executor::execute`]. Use the builder methods to
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
    executor: &'a Executor,
    code: String,
    preamble: String,
    callbacks: Vec<Arc<dyn Callback>>,
    callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    net_tx: Option<mpsc::Sender<NetRequest>>,
    output_tx: Option<mpsc::UnboundedSender<OutputRequest>>,
    memory_limit: Option<u64>,
    execution_timeout: Option<Duration>,
    cancellation_token: Option<CancellationToken>,
    fuel_limit: Option<u64>,
    max_child_host_bytes: Option<u64>,
    child_net: Option<crate::process::ChildNet>,
    allowed_commands: Option<Vec<String>>,
    command_options: HashMap<String, crate::process::CommandOptions>,
    credentials: Vec<Arc<crate::net::Credential>>,
    #[cfg(feature = "unstable-vfs")]
    vfs_storage: Option<eryx_vfs::ArcStorage>,
    #[cfg(feature = "unstable-vfs")]
    volumes: Vec<crate::session::VolumeMount>,
    #[cfg(feature = "unstable-vfs")]
    max_open_host_files: Option<usize>,
    #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
    vfs_limits: crate::session::executor::VfsLimits,
}

impl std::fmt::Debug for ExecuteBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ExecuteBuilder");
        debug
            .field("code_len", &self.code.len())
            .field("callbacks_count", &self.callbacks.len())
            .field("has_callback_tx", &self.callback_tx.is_some())
            .field("has_trace_tx", &self.trace_tx.is_some())
            .field("has_net_tx", &self.net_tx.is_some())
            .field("has_output_tx", &self.output_tx.is_some())
            .field("memory_limit", &self.memory_limit)
            .field("execution_timeout", &self.execution_timeout)
            .field("has_cancellation_token", &self.cancellation_token.is_some())
            .field("fuel_limit", &self.fuel_limit);
        #[cfg(feature = "unstable-vfs")]
        debug.field("has_vfs_storage", &self.vfs_storage.is_some());
        #[cfg(feature = "unstable-vfs")]
        debug.field("volumes_count", &self.volumes.len());
        debug.finish_non_exhaustive()
    }
}

impl<'a> ExecuteBuilder<'a> {
    /// Create a new execute builder.
    fn new(executor: &'a Executor, code: impl Into<String>) -> Self {
        Self {
            executor,
            code: code.into(),
            preamble: String::new(),
            callbacks: Vec::new(),
            callback_tx: None,
            trace_tx: None,
            net_tx: None,
            output_tx: None,
            memory_limit: None,
            execution_timeout: None,
            cancellation_token: None,
            fuel_limit: None,
            max_child_host_bytes: None,
            child_net: None,
            allowed_commands: None,
            command_options: HashMap::new(),
            credentials: Vec::new(),
            #[cfg(feature = "unstable-vfs")]
            vfs_storage: None,
            #[cfg(feature = "unstable-vfs")]
            volumes: Vec::new(),
            #[cfg(feature = "unstable-vfs")]
            max_open_host_files: None,
            #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
            vfs_limits: crate::session::executor::VfsLimits::default(),
        }
    }

    unstable_pub! {
    /// Set callbacks that Python code can invoke.
    ///
    /// The `callback_tx` channel is used to send callback requests from
    /// the WASM guest to the host for processing. Both the callbacks and
    /// the channel are required together since they work in tandem.
    #[must_use]
    fn with_callbacks(
        mut self,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: mpsc::Sender<CallbackRequest>,
    ) -> Self {
        self.callbacks = callbacks.to_vec();
        self.callback_tx = Some(callback_tx);
        self
    }
    }

    unstable_pub! {
    /// Set the trace channel for receiving execution trace events.
    #[must_use]
    fn with_tracing(mut self, trace_tx: mpsc::UnboundedSender<TraceRequest>) -> Self {
        self.trace_tx = Some(trace_tx);
        self
    }
    }

    unstable_pub! {
    /// Enable networking for this execution.
    ///
    /// The `net_tx` channel is used to send network requests (TCP/TLS) from
    /// the WASM guest to the host for processing.
    #[must_use]
    fn with_network(mut self, net_tx: mpsc::Sender<NetRequest>) -> Self {
        self.net_tx = Some(net_tx);
        self
    }
    }

    unstable_pub! {
    /// Enable real-time output streaming for this execution.
    ///
    /// The `output_tx` channel receives `OutputRequest`s for every
    /// `sys.stdout.write()` / `sys.stderr.write()` call in Python.
    #[must_use]
    fn with_output_streaming(
        mut self,
        output_tx: mpsc::UnboundedSender<OutputRequest>,
    ) -> Self {
        self.output_tx = Some(output_tx);
        self
    }
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

    /// Set VFS storage with scrubbing for secret placeholders.
    #[cfg(feature = "unstable-vfs")]
    #[must_use]
    pub fn with_vfs_storage(mut self, storage: eryx_vfs::ArcStorage) -> Self {
        self.vfs_storage = Some(storage);
        self
    }

    /// Set host filesystem volume mounts.
    #[cfg(feature = "unstable-vfs")]
    #[must_use]
    pub fn with_volumes(mut self, volumes: Vec<crate::session::VolumeMount>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Limit how many host files the execution may hold open (default
    /// [`eryx_vfs::DEFAULT_MAX_OPEN_HOST_FILES`]).
    #[cfg(feature = "unstable-vfs")]
    #[must_use]
    pub fn with_max_open_host_files(mut self, max: Option<usize>) -> Self {
        self.max_open_host_files = max;
        self
    }

    /// Limit the size and entry count of the shell's private `/tmp` (see
    /// [`ResourceLimits::max_vfs_bytes`](crate::ResourceLimits::max_vfs_bytes)).
    #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
    #[must_use]
    pub(crate) fn with_vfs_limits(mut self, limits: crate::session::executor::VfsLimits) -> Self {
        self.vfs_limits = limits;
        self
    }

    /// Limit the host memory child processes may hold (default 64 MiB; see
    /// [`ResourceLimits::max_child_host_bytes`](crate::ResourceLimits::max_child_host_bytes)).
    #[must_use]
    pub fn with_max_child_host_bytes(mut self, bytes: Option<u64>) -> Self {
        self.max_child_host_bytes = bytes;
        self
    }

    /// Give this execution's child processes network, through a proxy that
    /// lives as long as the execution.
    pub(crate) fn with_child_network(mut self, net: crate::process::ChildNet) -> Self {
        self.child_net = Some(net);
        self
    }

    /// Let this execution spawn only the named host-registered commands
    /// (shell only): the guest lists just those, and any other spawn fails
    /// with `command-not-found`. An empty list allows none; names that aren't
    /// registered are ignored. Without this, every registered command is
    /// allowed. The shell's `grep` builtin and nested `sh`/`bash` count as
    /// commands here; other builtins and callbacks aren't affected.
    #[must_use]
    pub fn with_allowed_commands<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_commands = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Change what the registered command `name` gets in this execution
    /// (shell only); see
    /// [`ExecuteOptions::with_command_options`](crate::ExecuteOptions::with_command_options).
    #[must_use]
    pub fn with_command_options(
        mut self,
        name: impl Into<String>,
        options: crate::process::CommandOptions,
    ) -> Self {
        self.command_options.insert(name.into(), options);
        self
    }

    /// Give this execution's children `credential` too (shell only); see
    /// [`ExecuteOptions::with_credential`](crate::ExecuteOptions::with_credential).
    #[must_use]
    pub fn with_credential(mut self, credential: crate::net::Credential) -> Self {
        self.credentials.push(Arc::new(credential));
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
                &frame_code(&self.preamble, &self.code),
                &self.callbacks,
                self.callback_tx,
                self.trace_tx,
                self.net_tx,
                self.output_tx,
                self.memory_limit,
                self.execution_timeout,
                self.cancellation_token,
                self.fuel_limit,
                self.max_child_host_bytes,
                self.child_net,
                self.allowed_commands.as_deref(),
                &self.command_options,
                &self.credentials,
                #[cfg(feature = "unstable-vfs")]
                self.vfs_storage,
                #[cfg(feature = "unstable-vfs")]
                self.volumes,
                #[cfg(feature = "unstable-vfs")]
                self.max_open_host_files,
                #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
                self.vfs_limits,
            )
            .await
    }
}

/// The shell guest's baseline environment (#832): where tools expect to find
/// a home, temp dir, locale and terminal. `/tmp` is the only directory it can
/// write to by default, so it is also `HOME`; `/eryx/bin` is where the guest
/// lists its host commands.
pub(crate) const SHELL_ENV: [(&str, &str); 8] = [
    ("HOME", "/tmp"),
    ("PATH", "/eryx/bin"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("USER", "eryx"),
    ("PWD", "/"),
    ("TMPDIR", "/tmp"),
];

/// Set [`SHELL_ENV`] on a shell guest's WASI context.
pub(crate) fn add_shell_env(builder: &mut WasiCtxBuilder) {
    for (key, value) in SHELL_ENV {
        builder.env(key, value);
    }
}

/// Frame `code` for the guest's `execute` export, with `preamble` to run first.
///
/// Every execution is framed, even without a preamble: the guest splits off
/// exactly one leading header, so a header-like first line in user code is
/// never mistaken for the framing. Must match the guest's `PREAMBLE_HEADER`.
pub(crate) fn frame_code(preamble: &str, code: &str) -> String {
    format!("#eryx-preamble:{}\n{preamble}{code}", preamble.len())
}

/// Guest mount path for the `index`-th site-packages directory.
///
/// Index 0 is `/site-packages`, the path a pre-initialized snapshot may have
/// baked into `sys.path`; later directories get an index suffix. The guest
/// looks for mounts in this order, and native extensions' dlopen paths must
/// use the same scheme.
pub(crate) fn site_packages_mount_path(index: usize) -> String {
    if index == 0 {
        "/site-packages".to_string()
    } else {
        format!("/site-packages-{index}")
    }
}

/// The executor that manages a guest language's WASM runtime.
///
/// This struct uses pre-instantiation (`SandboxPre`) to perform as much
/// work as possible upfront. The expensive operations (parsing WASM,
/// compiling, linking) happen once during construction. Each `execute()`
/// call only needs to create a new store and instantiate from the
/// pre-compiled template, which is much faster.
pub struct Executor {
    /// The wasmtime engine (shared configuration).
    engine: Engine,
    /// Pre-instantiated component - linking is already done.
    instance_pre: SandboxPre<ExecutorState>,
    /// The guest language this executor's runtime runs.
    language: Language,
    /// Path to the guest's standard library directory (for Python, the
    /// CPython stdlib it needs to initialize).
    stdlib_path: Option<PathBuf>,
    /// Paths to the guest's library directories (for Python, site-packages).
    /// Each will be mounted at `/site-packages-N` and added to PYTHONPATH.
    library_paths: Vec<PathBuf>,
    /// An environment's root, mounted read-only at
    /// [`crate::environment::MOUNT_PATH`].
    #[cfg(feature = "environment")]
    environment: Option<PathBuf>,
    /// Name of the user variable captured as the structured result after each
    /// execution. Defaults to `"result"`. Configured via [`Self::with_result_variable`].
    result_variable: String,
    /// Extracted packages backing entries in `library_paths`.
    /// Held here so their temp directories live as long as the executor,
    /// including when it outlives the `Sandbox` (e.g. in a session).
    packages: Vec<crate::package::ExtractedPackage>,
    /// External commands its guest can spawn (the shell's, via `eryx:process`).
    commands: crate::process::Commands,
    /// How the commands' children reach the network.
    child_network: crate::net::ChildNetwork,
    /// Credentials every execution's children get (see
    /// [`Credential`](crate::net::Credential)).
    credentials: Vec<Arc<crate::net::Credential>>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("engine", &"<wasmtime::Engine>")
            .field("instance_pre", &"<SandboxPre>")
            .field("language", &self.language)
            .field("stdlib_path", &self.stdlib_path)
            .field("library_paths", &self.library_paths)
            .field("result_variable", &self.result_variable)
            .finish_non_exhaustive()
    }
}

impl Executor {
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

    /// Number of pre-instantiated stores currently waiting for this
    /// executor's component.
    ///
    /// Stateless executions take one of these instead of instantiating, and a
    /// background task replaces it afterwards. The pool is shared by every
    /// executor loading the same component and sized by
    /// [`SharedEngineConfig::with_warm_instances`] or `ERYX_WARM_INSTANCES` (default 1,
    /// `0` disables it); it is only used on a multi-threaded Tokio
    /// runtime.
    #[must_use]
    pub fn warm_instances_ready(&self) -> usize {
        WarmPool::global().ready(&self.instance_pre)
    }

    /// The guest language this executor runs (default [`Language::Python`]).
    #[must_use]
    pub fn language(&self) -> Language {
        self.language
    }

    /// The guest's standard library directory, if configured.
    #[must_use]
    pub fn stdlib_path(&self) -> Option<&PathBuf> {
        self.stdlib_path.as_ref()
    }

    /// Deprecated name for [`stdlib_path`](Self::stdlib_path).
    #[deprecated(since = "0.16.0", note = "renamed to `stdlib_path`")]
    #[must_use]
    pub fn python_stdlib_path(&self) -> Option<&PathBuf> {
        self.stdlib_path()
    }

    /// The guest's library directories (for Python, site-packages).
    #[must_use]
    pub fn library_paths(&self) -> &[PathBuf] {
        &self.library_paths
    }

    /// Deprecated name for [`library_paths`](Self::library_paths).
    #[deprecated(since = "0.16.0", note = "renamed to `library_paths`")]
    #[must_use]
    pub fn python_site_packages_paths(&self) -> &[PathBuf] {
        self.library_paths()
    }

    /// Get the configured result-capture variable name (defaults to `"result"`).
    #[must_use]
    pub fn result_variable(&self) -> &str {
        &self.result_variable
    }

    /// Set the guest language this executor's runtime runs.
    ///
    /// The runtime itself decides what runs; this records which language it
    /// is, so state snapshots are stamped with it and checked on restore.
    #[must_use]
    pub fn with_language(mut self, language: Language) -> Self {
        self.language = language;
        self
    }

    /// Set the path to the guest's standard library directory.
    ///
    /// For Python, the directory should contain the extracted CPython stdlib
    /// (e.g., from componentize-py's python-lib.tar.zst). It is mounted at
    /// `/python-stdlib` inside the WASM sandbox.
    #[must_use]
    pub fn with_stdlib(mut self, path: impl Into<PathBuf>) -> Self {
        self.stdlib_path = Some(path.into());
        self
    }

    /// Deprecated name for [`with_stdlib`](Self::with_stdlib).
    #[deprecated(since = "0.16.0", note = "renamed to `with_stdlib`")]
    #[must_use]
    pub fn with_python_stdlib(self, path: impl Into<PathBuf>) -> Self {
        self.with_stdlib(path)
    }

    /// Add a guest library directory (for Python, a site-packages directory).
    ///
    /// The first directory will be mounted at `/site-packages` inside the WASM sandbox
    /// (for compatibility with preinit). Additional directories are mounted at
    /// `/site-packages-1`, `/site-packages-2`, etc. All paths are added to Python's
    /// import path. Can be called multiple times.
    #[must_use]
    pub fn with_library_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.library_paths.push(path.into());
        self
    }

    /// Mount the environment at `root` read-only at
    /// [`crate::environment::MOUNT_PATH`]. `SandboxBuilder::with_environment`
    /// checks it first.
    #[cfg(feature = "environment")]
    #[must_use]
    pub(crate) fn with_environment(mut self, root: &std::path::Path) -> Self {
        self.environment = Some(root.to_path_buf());
        self
    }

    /// The environment root this executor mounts, if any.
    #[cfg(feature = "environment")]
    pub(crate) fn environment(&self) -> Option<&std::path::Path> {
        self.environment.as_deref()
    }

    /// Deprecated name for [`with_library_path`](Self::with_library_path).
    #[deprecated(since = "0.16.0", note = "renamed to `with_library_path`")]
    #[must_use]
    pub fn with_site_packages(self, path: impl Into<PathBuf>) -> Self {
        self.with_library_path(path)
    }

    /// Keep extracted packages alive for as long as this executor exists.
    #[must_use]
    pub(crate) fn with_packages(mut self, packages: Vec<crate::package::ExtractedPackage>) -> Self {
        self.packages = packages;
        self
    }

    /// Set the name of the user variable captured as the structured result.
    ///
    /// After each `execute()`, the variable with this name is read from the user
    /// namespace, JSON-serialized, and returned in [`ExecutionOutput::result`].
    /// Defaults to `"result"`.
    #[must_use]
    pub fn with_result_variable(mut self, name: impl Into<String>) -> Self {
        self.result_variable = name.into();
        self
    }

    /// The external commands its guest can spawn.
    pub(crate) fn with_commands(mut self, commands: crate::process::Commands) -> Self {
        self.commands = commands;
        self
    }

    /// The external commands its guest can spawn.
    pub(crate) fn commands(&self) -> &crate::process::Commands {
        &self.commands
    }

    /// How its commands' children reach the network.
    pub(crate) fn with_child_network(mut self, network: crate::net::ChildNetwork) -> Self {
        self.child_network = network;
        self
    }

    /// How its commands' children reach the network.
    pub(crate) fn child_network(&self) -> &crate::net::ChildNetwork {
        &self.child_network
    }

    /// Credentials every execution's children get.
    pub(crate) fn with_credentials(
        mut self,
        credentials: Vec<Arc<crate::net::Credential>>,
    ) -> Self {
        self.credentials = credentials;
        self
    }

    /// Credentials every execution's children get.
    pub(crate) fn credentials(&self) -> &[Arc<crate::net::Credential>] {
        &self.credentials
    }

    /// Get or create the global shared wasmtime Engine.
    ///
    /// The Engine is thread-safe and automatically shared across all `Executor`
    /// instances. Sharing an Engine saves memory and startup time since the JIT
    /// compiler configuration and compiled code cache are shared.
    ///
    /// This is called automatically by `from_binary`, `from_file`, etc.
    /// You typically don't need to call this directly.
    ///
    /// # Errors
    ///
    /// Returns an error if engine creation fails. A later call will retry
    /// initialization after a transient failure.
    pub fn shared_engine() -> std::result::Result<Engine, Error> {
        static SHARED_ENGINE: RetryableOnce<Engine> = RetryableOnce::new();

        SHARED_ENGINE.get_or_try_init(Self::create_engine).cloned()
    }

    /// The engine external commands (shell children) are compiled for and run
    /// on: the shared engine's configuration with on-demand allocation, so
    /// children never take the pooling allocator's slots, which stay sized
    /// for sandboxes (#833). Created on first use; when the shared engine is
    /// already on-demand (`ERYX_ALLOCATOR=on-demand`, or Windows), it's that
    /// engine.
    ///
    /// Both engines' compiled code is interchangeable: the allocator isn't
    /// part of wasmtime's compatibility check, so a command precompiled by
    /// [`Self::precompile`] loads on either.
    pub(crate) fn child_engine() -> std::result::Result<Engine, Error> {
        static CHILD_ENGINE: RetryableOnce<Engine> = RetryableOnce::new();

        CHILD_ENGINE
            .get_or_try_init(|| {
                let shared = Self::shared_engine()?;
                if shared.pooling_allocator_metrics().is_none() {
                    return Ok(shared);
                }
                let mut config = Self::engine_config_with_target(None)?;
                config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
                Engine::new(&config).map_err(|e| Error::from(ErrorKind::WasmEngine(e.to_string())))
            })
            .cloned()
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
        name = "Executor::from_binary",
        skip(wasm_bytes),
        fields(wasm_bytes_len = wasm_bytes.len())
    )]
    pub fn from_binary(wasm_bytes: &[u8]) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;
        let component =
            Component::from_binary(&engine, wasm_bytes).map_err(ErrorKind::wasm_component)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        Ok(Self {
            engine,
            instance_pre,
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
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
        name = "Executor::from_file",
        fields(path = %path.as_ref().display())
    )]
    pub fn from_file(path: impl AsRef<std::path::Path>) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;
        let component =
            Component::from_file(&engine, path.as_ref()).map_err(ErrorKind::wasm_component)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        Ok(Self {
            engine,
            instance_pre,
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
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
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled(precompiled_bytes: &[u8]) -> std::result::Result<Self, Error> {
        // With embedded feature, delegate to internal method for caching support
        #[cfg(feature = "embedded")]
        {
            #[allow(unsafe_code)]
            unsafe {
                Self::from_precompiled_internal(precompiled_bytes, None)
            }
        }
        // With preinit-only, load directly without caching
        #[cfg(not(feature = "embedded"))]
        {
            let engine = Self::shared_engine()?;
            // SAFETY: Caller guarantees the precompiled bytes are trusted and were
            // created by `precompile()` with a compatible engine configuration.
            let component = unsafe { Component::deserialize(&engine, precompiled_bytes) }
                .map_err(deserialize_error)?;
            let instance_pre = Self::create_instance_pre(&engine, &component)?;

            Ok(Self {
                engine,
                instance_pre,
                stdlib_path: None,
                library_paths: Vec::new(),
                #[cfg(feature = "environment")]
                environment: None,
                language: Language::Python,
                result_variable: "result".to_string(),
                commands: crate::process::Commands::default(),
                child_network: crate::net::ChildNetwork::None,
                credentials: Vec::new(),
                packages: Vec::new(),
            })
        }
    }

    /// Create a new executor by loading a pre-compiled component from bytes,
    /// with caching via [`InstancePreCache`].
    ///
    /// When a `cache_key` is provided:
    /// - First checks the global [`InstancePreCache`] for a cached `SandboxPre`
    /// - On cache hit, returns immediately without deserializing (~0ms)
    /// - On cache miss, deserializes the bytes and stores in cache for future use
    ///
    /// This is the internal method used by the sandbox builder for cached
    /// executor creation from pre-compiled bytes.
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate pre-compiled
    /// components for safety. Only use this with bytes you control and trust.
    ///
    /// # Errors
    ///
    /// Returns an error if the pre-compiled bytes are invalid or incompatible
    /// with the current engine configuration.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_precompiled_with_key(
        precompiled_bytes: &[u8],
        cache_key: CacheKey,
    ) -> std::result::Result<Self, Error> {
        #[allow(unsafe_code)]
        unsafe {
            Self::from_precompiled_internal(precompiled_bytes, Some(cache_key))
        }
    }

    /// Internal implementation for loading from pre-compiled bytes with optional caching.
    #[cfg(feature = "embedded")]
    #[allow(unsafe_code)]
    unsafe fn from_precompiled_internal(
        precompiled_bytes: &[u8],
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
                stdlib_path: None,
                library_paths: Vec::new(),
                #[cfg(feature = "environment")]
                environment: None,
                language: Language::Python,
                result_variable: "result".to_string(),
                commands: crate::process::Commands::default(),
                child_network: crate::net::ChildNetwork::None,
                credentials: Vec::new(),
                packages: Vec::new(),
            });
        }

        // Cache miss - deserialize from bytes and create instance_pre
        // SAFETY: Caller guarantees the precompiled bytes are trusted and were
        // created by `precompile()` with a compatible engine configuration.
        #[allow(unsafe_code)]
        let component = unsafe { Component::deserialize(&engine, precompiled_bytes) }
            .map_err(deserialize_error)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        // Store in cache if we have a key
        if let Some(key) = cache_key {
            InstancePreCache::global().put(key, instance_pre.clone());
        }

        Ok(Self {
            engine,
            instance_pre,
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
        })
    }

    /// Memory-map a pre-compiled component from a file with the shared engine.
    ///
    /// Wasmtime ignores any bytes after the component, so the file may carry
    /// trailing data.
    ///
    /// # Safety
    ///
    /// The file must hold trusted output of `precompile()`, and must not be
    /// modified while the returned component is alive.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn deserialize_component_file(
        path: &std::path::Path,
    ) -> std::result::Result<DeserializedComponent, Error> {
        let engine = Self::shared_engine()?;
        // SAFETY: Caller guarantees the file is trusted and stays unmodified.
        let component =
            unsafe { Component::deserialize_file(&engine, path) }.map_err(deserialize_error)?;
        Ok(DeserializedComponent {
            component,
            instance_pre: RetryableOnce::new(),
        })
    }

    /// Create a new executor from a component returned by
    /// [`Self::deserialize_component_file`].
    ///
    /// The component is linked once and the result reused by every later
    /// call. With a cache key (`embedded` only) the process-global
    /// instance-pre cache is consulted first and filled afterwards.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    pub(crate) fn from_deserialized_component(
        deserialized: &DeserializedComponent,
        cache_key: Option<crate::cache::CacheKey>,
    ) -> std::result::Result<Self, Error> {
        let engine = Self::shared_engine()?;

        #[cfg(feature = "embedded")]
        let cached = cache_key
            .as_ref()
            .and_then(|key| InstancePreCache::global().get(key));
        #[cfg(not(feature = "embedded"))]
        let cached = None;

        let instance_pre = match cached {
            Some(instance_pre) => instance_pre,
            None => {
                let instance_pre = deserialized
                    .instance_pre
                    .get_or_try_init(|| {
                        Self::create_instance_pre(&engine, &deserialized.component)
                    })?
                    .clone();
                #[cfg(feature = "embedded")]
                if let Some(key) = cache_key {
                    InstancePreCache::global().put(key, instance_pre.clone());
                }
                #[cfg(not(feature = "embedded"))]
                let _ = cache_key;
                instance_pre
            }
        };

        Ok(Self {
            engine,
            instance_pre,
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
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
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled_file(
        path: impl AsRef<std::path::Path>,
    ) -> std::result::Result<Self, Error> {
        // With embedded feature, delegate to internal method for caching support
        #[cfg(feature = "embedded")]
        {
            #[allow(unsafe_code)]
            unsafe {
                Self::from_precompiled_file_internal(path.as_ref(), None)
            }
        }
        // With preinit-only, load directly without caching
        #[cfg(not(feature = "embedded"))]
        {
            let engine = Self::shared_engine()?;
            // SAFETY: Caller guarantees the precompiled file is trusted
            #[allow(unsafe_code)]
            let component = unsafe { Component::deserialize_file(&engine, path.as_ref()) }
                .map_err(deserialize_error)?;
            let instance_pre = Self::create_instance_pre(&engine, &component)?;
            Ok(Self {
                engine,
                instance_pre,
                stdlib_path: None,
                library_paths: Vec::new(),
                #[cfg(feature = "environment")]
                environment: None,
                language: Language::Python,
                result_variable: "result".to_string(),
                commands: crate::process::Commands::default(),
                child_network: crate::net::ChildNetwork::None,
                credentials: Vec::new(),
                packages: Vec::new(),
            })
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
                stdlib_path: None,
                library_paths: Vec::new(),
                #[cfg(feature = "environment")]
                environment: None,
                language: Language::Python,
                result_variable: "result".to_string(),
                commands: crate::process::Commands::default(),
                child_network: crate::net::ChildNetwork::None,
                credentials: Vec::new(),
                packages: Vec::new(),
            });
        }

        // Cache miss - load from file and create instance_pre
        // SAFETY: Caller guarantees the precompiled file is trusted and was
        // created by `precompile()` with a compatible engine configuration.
        #[allow(unsafe_code)]
        let component =
            unsafe { Component::deserialize_file(&engine, path) }.map_err(deserialize_error)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;

        // Store in cache if we have a key
        if let Some(key) = cache_key {
            InstancePreCache::global().put(key, instance_pre.clone());
        }

        Ok(Self {
            engine,
            instance_pre,
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
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
    /// Use [`Self::with_stdlib`] and [`Self::with_library_path`] to
    /// configure paths after creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded resources cannot be extracted or
    /// the component cannot be loaded.
    #[cfg(feature = "embedded")]
    #[tracing::instrument(name = "Executor::from_embedded_runtime")]
    pub fn from_embedded_runtime() -> std::result::Result<Self, Error> {
        Self::from_embedded_runtime_for(Language::Python)
    }

    /// Create an executor from `language`'s embedded runtime, like
    /// [`Self::from_embedded_runtime`] does for Python.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::MissingRuntime`] if this build doesn't embed a
    /// runtime for `language`, or an error if it cannot be extracted or loaded.
    #[cfg(feature = "embedded")]
    pub fn from_embedded_runtime_for(language: Language) -> std::result::Result<Self, Error> {
        let cache_key = CacheKey::embedded_runtime_for(language);

        // Get embedded resources (for Python, extracts the stdlib on first call)
        let (runtime_path, stdlib_path) = if language == Language::Python {
            let resources = crate::embedded::EmbeddedResources::get()?;
            (resources.runtime(), Some(resources.stdlib_path.clone()))
        } else {
            let path = crate::embedded::runtime_path(language)?
                .ok_or(ErrorKind::MissingRuntime { language })?;
            (path, None)
        };

        // Check InstancePreCache first (fast path)
        if let Some(instance_pre) = InstancePreCache::global().get(&cache_key) {
            return Ok(Self {
                engine: Self::shared_engine()?,
                instance_pre,
                stdlib_path,
                library_paths: Vec::new(),
                #[cfg(feature = "environment")]
                environment: None,
                language,
                result_variable: "result".to_string(),
                commands: crate::process::Commands::default(),
                child_network: crate::net::ChildNetwork::None,
                credentials: Vec::new(),
                packages: Vec::new(),
            });
        }

        // Cache miss - load from embedded resources. A stale runtime embedded
        // through ERYX_RUNTIME_CWASM* fails here rather than misbehaving.
        if let Some(stamp) = std::fs::File::open(runtime_path).ok().and_then(|mut file| {
            let len = file.metadata().ok()?.len();
            file_language_stamp(&mut file, len)
        }) {
            stamp.check_guest()?;
        }
        // SAFETY: The embedded runtime was pre-compiled at build time from our own
        // trusted runtime-<language>.wasm, so we know it's safe to deserialize.
        #[allow(unsafe_code)]
        let mut executor =
            unsafe { Self::from_precompiled_file_with_key(runtime_path, cache_key)? };

        executor.stdlib_path = stdlib_path;
        executor.language = language;
        Ok(executor)
    }

    /// An executor that runs the `language` runtime precompiled at `path` as
    /// a guest command (see [`crate::Command::guest`]), loaded on the child
    /// engine so guest children never take the sandboxes' pooling slots.
    ///
    /// # Safety
    ///
    /// As for [`Self::from_precompiled_file`]: a trusted file that doesn't
    /// change while it's loaded.
    #[cfg(all(
        any(feature = "embedded", feature = "preinit"),
        any(feature = "unstable-vfs", feature = "shell")
    ))]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn for_guest_command(
        language: Language,
        path: &std::path::Path,
        stdlib_path: Option<PathBuf>,
    ) -> std::result::Result<Self, Error> {
        if let Some(stamp) = std::fs::File::open(path).ok().and_then(|mut file| {
            let len = file.metadata().ok()?.len();
            file_language_stamp(&mut file, len)
        }) {
            if stamp.language != language {
                return Err(ErrorKind::Initialization(format!(
                    "this precompiled runtime runs {}, not {language}",
                    stamp.language
                ))
                .into());
            }
            stamp.check_guest()?;
        }
        let engine = Self::child_engine()?;
        // SAFETY: the caller vouches for the file.
        let component =
            unsafe { Component::deserialize_file(&engine, path) }.map_err(deserialize_error)?;
        let instance_pre = Self::create_instance_pre(&engine, &component)?;
        Ok(Self {
            engine,
            instance_pre,
            stdlib_path,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
        })
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
            stdlib_path: None,
            library_paths: Vec::new(),
            #[cfg(feature = "environment")]
            environment: None,
            language: Language::Python,
            result_variable: "result".to_string(),
            commands: crate::process::Commands::default(),
            child_network: crate::net::ChildNetwork::None,
            credentials: Vec::new(),
            packages: Vec::new(),
        })
    }

    /// Pre-compile the WASM component to native code for faster loading.
    ///
    /// The returned bytes can be saved to a file (conventionally with `.cwasm`
    /// extension) and later loaded with `from_precompiled` or `from_precompiled_file`.
    ///
    /// The output records this eryx version and only loads in it. Loading it
    /// in any other version fails with [`ErrorKind::IncompatiblePrecompiled`], so
    /// precompiled files must be rebuilt after every eryx upgrade.
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
        Self::check_cpu_feature_env()?;
        let engine = Self::create_engine_with_target(None)?;
        engine.precompile_component(wasm_bytes).map_err(|e| {
            ErrorKind::WasmEngine(format!("Failed to precompile component: {e}")).into()
        })
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
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to read WASM file: {e}")))?;
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
        Self::check_cpu_feature_env()?;
        let engine = Self::create_engine_with_target(target)?;
        engine.precompile_component(wasm_bytes).map_err(|e| {
            ErrorKind::WasmEngine(format!("Failed to precompile component: {e}")).into()
        })
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
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to read WASM file: {e}")))?;
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
    /// use eryx::{Executor, CpuFeatureLevel};
    ///
    /// // Compile for Fly.io (x86-64-v3, no AVX-512)
    /// let cwasm = Executor::precompile_with_options(
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
        engine.precompile_component(wasm_bytes).map_err(|e| {
            ErrorKind::WasmEngine(format!("Failed to precompile component: {e}")).into()
        })
    }

    /// Stamp `precompiled` (output of the `precompile` functions) with the
    /// guest language it runs, so that building a sandbox for another
    /// language from it fails with a clear error instead of misbehaving.
    /// Unstamped runtimes are not checked.
    ///
    /// An eryx built from a checkout also stamps the hash of the guest's
    /// sources it was built with. Loading the runtime in an eryx built from
    /// other guest sources then fails with
    /// [`ErrorKind::IncompatiblePrecompiled`], as for another eryx version.
    pub fn stamp_language(precompiled: &mut Vec<u8>, language: Language) {
        let magic = match guest_sources_hash(language) {
            Some(hash) => {
                precompiled.extend_from_slice(&hash.to_le_bytes());
                GUEST_STAMP_MAGIC
            }
            None => LANGUAGE_STAMP_MAGIC,
        };
        precompiled.push(language.snapshot_tag());
        precompiled.extend_from_slice(magic);
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
    /// - v4: Store eryx's version in precompiled components
    pub const ENGINE_CONFIG_VERSION: u32 = 4;

    /// Stack the guest's wasm code may use: wasmtime's default.
    const MAX_WASM_STACK: usize = 512 * 1024;

    /// Size of the async fiber the guest runs on.
    ///
    /// Host code called from the guest (WASI, the VFS, callback and net dispatch)
    /// runs on the same fiber and only gets `ASYNC_STACK_SIZE - MAX_WASM_STACK`
    /// of it. With no headroom, deep guest recursion overflows the fiber
    /// before the wasm stack limit can trap, and wasmtime aborts the whole
    /// process. 512 KiB of headroom was enough for every host path audited.
    ///
    /// This is half wasmtime's 2 MiB default because a 2 MiB stack mapping is
    /// eligible for a transparent huge page: under the on-demand allocator
    /// with THP enabled that costs about 1.5 MB of RSS per live sandbox.
    const ASYNC_STACK_SIZE: usize = 1024 * 1024;

    /// Minimum host headroom on the fiber; see [`Self::ASYNC_STACK_SIZE`].
    const MIN_HOST_STACK: usize = 512 * 1024;

    /// The version string eryx stores in every precompiled component, e.g.
    /// `eryx 0.13.0 (wasmtime 50)`.
    ///
    /// wasmtime refuses to load a component whose stored string differs from
    /// the engine's. Its default string is only wasmtime's major version, but a
    /// precompiled runtime also depends on eryx's WIT interface, linked
    /// libraries and engine config, any of which can change in an eryx
    /// release. Naming the eryx version makes a stale file fail with
    /// [`ErrorKind::IncompatiblePrecompiled`] instead of a type-checking error.
    pub(crate) fn module_version() -> String {
        format!(
            "eryx {} (wasmtime {})",
            env!("CARGO_PKG_VERSION"),
            ModuleVersionStrategy::default().as_str()
        )
    }

    /// Create a configured wasmtime engine.
    ///
    /// Uses copy-on-write heap images to defer memory initialization
    /// from instantiation time to first write, improving startup performance.
    ///
    /// This is the engine that runs sandboxes, so it is the only one that uses
    /// the pooling allocator; the precompile engines only compile.
    fn create_engine() -> std::result::Result<Engine, Error> {
        Self::build_engine(Self::engine_config_with_target(None)?)
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
    /// - `baseline` - The target architecture's baseline, no optional features.
    ///   The only portable choice on aarch64, which has no psABI presets.
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
    /// Refuse to precompile when `ERYX_CPU_FEATURES` or `ERYX_CRANELIFT_FLAGS`
    /// is set but this build can't apply it (it needs the `embedded` or
    /// `preinit` feature): the output would silently target this machine's
    /// CPU instead, and fail to load on one with fewer features.
    fn check_cpu_feature_env() -> std::result::Result<(), Error> {
        #[cfg(not(any(feature = "embedded", feature = "preinit")))]
        for var in ["ERYX_CPU_FEATURES", "ERYX_CRANELIFT_FLAGS"] {
            if std::env::var_os(var).is_some() {
                return Err(ErrorKind::WasmEngine(format!(
                    "{var} is set, but this build of eryx can't apply it: enable \
                     the `embedded` or `preinit` feature"
                ))
                .into());
            }
        }
        Ok(())
    }

    fn create_engine_with_target(target: Option<&str>) -> std::result::Result<Engine, Error> {
        let config = Self::engine_config_with_target(target)?;
        Engine::new(&config).map_err(|e| ErrorKind::WasmEngine(e.to_string()).into())
    }

    /// The configuration [`create_engine_with_target`](Self::create_engine_with_target)
    /// builds an engine from.
    fn engine_config_with_target(target: Option<&str>) -> std::result::Result<Config, Error> {
        let mut config = Self::base_engine_config()?;

        // Configure target triple for cross-compilation or portable builds.
        // Check explicit parameter first, then environment variable, then use native.
        let effective_target = target
            .map(|s| s.to_string())
            .or_else(|| std::env::var("ERYX_TARGET").ok());

        if let Some(ref target_str) = effective_target {
            config.target(target_str).map_err(|e| {
                ErrorKind::WasmEngine(format!("Invalid target '{target_str}': {e}"))
            })?;
        }

        // Apply CPU feature level preset and custom Cranelift flags.
        // These require unsafe code, so they're only available with embedded or preinit features.
        #[cfg(any(feature = "embedded", feature = "preinit"))]
        Self::apply_cpu_feature_flags(&mut config, effective_target.as_deref())?;

        Ok(config)
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
        let mut config = Self::base_engine_config()?;

        // Target triple and CPU features are set together: pinning a triple is
        // also what stops Cranelift inferring features from the build machine.
        Self::apply_cpu_feature_level(&mut config, cpu_features, target)?;

        Engine::new(&config).map_err(|e| ErrorKind::WasmEngine(e.to_string()).into())
    }

    /// The engine options shared by every engine eryx creates.
    ///
    /// Options that affect the generated code (wasm features, fuel, epochs,
    /// Cranelift settings, the module version) must be paired with a bump of
    /// [`ENGINE_CONFIG_VERSION`](Self::ENGINE_CONFIG_VERSION) when changed.
    /// Runtime-only settings, such as the stack sizes, the trap handler and
    /// the allocation strategy, are not part of a precompiled component and
    /// need no bump.
    fn base_engine_config() -> std::result::Result<Config, Error> {
        let mut config = Config::new();
        config
            .module_version(ModuleVersionStrategy::Custom(Self::module_version()))
            .map_err(|e| ErrorKind::WasmEngine(e.to_string()))?;
        config.wasm_component_model(true);
        // Enable component model async for the `invoke` callback function.
        // The invoke function is async because Python code awaits on it.
        // TCP/TLS functions are sync `func` in WIT but use fiber-based async
        // on the host (via `async` bindgen flag) - they appear blocking to guest.
        config.wasm_component_model_async(true);
        // The synchronous `stream`/`future` read and write builtins and async
        // `subtask.cancel`, which wasip3 Go programs (`gh`) use as child
        // commands. Only validation gates them: wasmparser refuses those
        // canonical builtins unless this is on.
        config.wasm_component_model_more_async_builtins(true);

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

        // Guest code runs on an async fiber, and host code called from the
        // guest only gets the part of it above the wasm stack limit. See
        // `ASYNC_STACK_SIZE` for the rule; `tests/stack_overflow.rs` checks it
        // end to end.
        const { assert!(Self::ASYNC_STACK_SIZE >= Self::MAX_WASM_STACK + Self::MIN_HOST_STACK) };
        config.max_wasm_stack(Self::MAX_WASM_STACK);
        config.async_stack_size(Self::ASYNC_STACK_SIZE);

        // Use Unix signals rather than Mach ports for trap handling on macOS.
        // The Mach port handler thread does not survive `fork()`, so a forked
        // child (e.g. a Python multiprocessing worker) would panic on its
        // first wasm call. This only picks the host trap handler and is not
        // part of the compiled artifact, so it needs no version bump.
        // Trap handling is process-global, so every engine in the process,
        // including the pre-init one, must use the same setting.
        config.macos_use_mach_ports(false);

        Ok(config)
    }

    /// Build the engine, falling back to on-demand allocation if the pool
    /// cannot be reserved.
    ///
    /// The pooling allocator reserves virtual address space for every slot up
    /// front (terabytes with the default limits), which a host with a low
    /// `ulimit -v` or an exotic kernel can refuse. Execution works identically
    /// on either allocator, so that refusal only costs instantiation latency.
    fn build_engine(mut config: Config) -> std::result::Result<Engine, Error> {
        let allocator = SharedEngineConfig::current().allocator_settings()?;
        let Some(pooling) = allocator.pooling_config() else {
            return Engine::new(&config).map_err(|e| ErrorKind::WasmEngine(e.to_string()).into());
        };

        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
        match Engine::new(&config) {
            Ok(engine) => {
                if let Some(interval) = allocator.release_interval {
                    start_pool_release(&engine, interval);
                }
                Ok(engine)
            }
            Err(pooling_err) => {
                tracing::warn!(
                    error = %pooling_err,
                    "failed to create wasmtime engine with the pooling allocator; \
                     falling back to on-demand allocation"
                );
                config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
                Engine::new(&config).map_err(|e| ErrorKind::WasmEngine(e.to_string()).into())
            }
        }
    }

    /// Pin the compilation target and enable the requested x86-64 psABI level.
    ///
    /// Cranelift infers every CPU feature flag from the build machine unless a
    /// target triple is set:
    ///
    /// ```text
    /// let triple_specified = triple.is_some();
    /// let triple = triple.unwrap_or_else(Triple::host);
    /// let mut isa_flags = lookup(triple)?;
    /// if !triple_specified {
    ///     cranelift_native::infer_native_flags(&mut isa_flags).unwrap();
    /// }
    /// ```
    ///
    /// This used to infer the host's flags and then *subtract* a hand-written
    /// list of "too new" ones, which meant any flag the list did not mention was
    /// silently inherited from whichever machine did the build. That is how
    /// `has_avx512vnni` ended up baked into a supposedly portable cwasm as soon
    /// as Cranelift started detecting it, breaking every host without VNNI.
    ///
    /// Setting a target triple — the host's own is fine, and keeps `Component`
    /// creation working — turns inference off, so every flag starts from the
    /// target's baseline. The level is then applied with Cranelift's own psABI
    /// presets, whose names are exactly [`CpuFeatureLevel::as_str`]. That makes
    /// this an allowlist: a feature Cranelift learns about in some future
    /// release stays off unless the psABI level actually includes it, so the
    /// cost of not knowing about it is a missed optimisation rather than an
    /// artifact that refuses to load.
    ///
    /// [`CpuFeatureLevel::Baseline`] pins the target and stops there, which is
    /// the only portable option on architectures Cranelift has no presets for.
    /// The psABI levels are x86-64 only, so asking for one against a non-x86-64
    /// target is rejected here rather than surfacing as an unknown-setting error
    /// from `Engine::new`.
    ///
    /// `explicit_target` is the caller's requested triple, if any; the host's is
    /// used otherwise.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn apply_cpu_feature_level(
        config: &mut Config,
        level: CpuFeatureLevel,
        explicit_target: Option<&str>,
    ) -> std::result::Result<(), Error> {
        // `Native` means "whatever this machine has", which is exactly what
        // Cranelift does when no target is pinned — including when the caller
        // asked for a specific triple, where it is the caller's business.
        if level == CpuFeatureLevel::Native {
            if let Some(target_str) = explicit_target {
                config.target(target_str).map_err(|e| {
                    ErrorKind::WasmEngine(format!("Invalid target '{target_str}': {e}"))
                })?;
            }
            return Ok(());
        }

        let triple = match explicit_target {
            Some(target_str) => {
                config.target(target_str).map_err(|e| {
                    ErrorKind::WasmEngine(format!("Invalid target '{target_str}': {e}"))
                })?;
                target_str.parse::<target_lexicon::Triple>().map_err(|e| {
                    ErrorKind::WasmEngine(format!("Invalid target '{target_str}': {e}"))
                })?
            }
            None => {
                let host = target_lexicon::Triple::host();
                config.target(&host.to_string()).map_err(|e| {
                    ErrorKind::WasmEngine(format!("Failed to pin host target '{host}': {e}"))
                })?;
                host
            }
        };

        // The target is pinned now, so Cranelift starts from its baseline and
        // never consults the build machine. `Baseline` wants exactly that and
        // nothing more.
        if level == CpuFeatureLevel::Baseline {
            return Ok(());
        }

        // Everything else is an x86-64 psABI level. Cranelift only defines those
        // presets for x86, so reject other architectures with something more
        // useful than "unknown setting" out of Engine::new.
        if triple.architecture != target_lexicon::Architecture::X86_64 {
            return Err(ErrorKind::WasmEngine(format!(
                "CPU feature level '{level}' is an x86-64 psABI level and does not \
                 apply to target '{triple}'. Use 'baseline' for a portable build \
                 or 'native' to use this machine's features."
            ))
            .into());
        }

        // SAFETY: `as_str` yields one of Cranelift's x86 psABI preset names.
        unsafe {
            config.cranelift_flag_enable(level.as_str());
        }

        Ok(())
    }

    /// Apply CPU feature level presets and custom Cranelift flags.
    ///
    /// This handles `ERYX_CPU_FEATURES` (preset levels) and `ERYX_CRANELIFT_FLAGS` (custom flags).
    /// Requires `embedded` or `preinit` feature for the unsafe Cranelift flag APIs.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn apply_cpu_feature_flags(
        config: &mut Config,
        target: Option<&str>,
    ) -> std::result::Result<(), Error> {
        // Apply CPU feature level preset from environment variable.
        // This provides an easy way to target specific x86-64 microarchitecture levels.
        if let Ok(level) = std::env::var("ERYX_CPU_FEATURES") {
            let level: CpuFeatureLevel = level.parse().map_err(ErrorKind::WasmEngine)?;
            // Pass the target on: with `None` the helper pins the host triple,
            // which would silently override a cross-compilation target.
            Self::apply_cpu_feature_level(config, level, target)?;
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
    #[tracing::instrument(name = "Executor::create_instance_pre", skip(engine, component))]
    fn create_instance_pre(
        engine: &Engine,
        component: &Component,
    ) -> std::result::Result<SandboxPre<ExecutorState>, Error> {
        let mut linker = Linker::<ExecutorState>::new(engine);

        // Add WASI support (p2 = preview 2)
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to add WASI to linker: {e}")))?;
        // And WASI 0.3, for guests built for wasm32-wasip3. Its interfaces
        // have their own versions, and a component links only what it
        // imports, so p2 guests are unaffected.
        wasmtime_wasi::p3::add_to_linker(&mut linker)
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to add WASI 0.3 to linker: {e}")))?;

        // Add hybrid VFS filesystem support (overrides WASI filesystem bindings)
        // The hybrid VFS routes:
        // - /data/* paths to VFS storage (sandboxed in-memory/KV store)
        // - Other paths (like /python-stdlib/*) to real filesystem via WASI
        // This allows Python to access its stdlib while providing sandboxed storage.
        // The shell always runs on it, for its private /tmp, /dev and root.
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        {
            // Allow shadowing to override WASI filesystem bindings
            linker.allow_shadowing(true);
            eryx_vfs::add_hybrid_vfs_to_linker(&mut linker).map_err(|e| {
                ErrorKind::WasmEngine(format!("Failed to add hybrid VFS to linker: {e}"))
            })?;
            eryx_vfs::add_hybrid_vfs_to_linker_p3(&mut linker).map_err(|e| {
                ErrorKind::WasmEngine(format!(
                    "Failed to add the WASI 0.3 hybrid VFS to linker: {e}"
                ))
            })?;
            linker.allow_shadowing(false);
        }

        // Add sandbox bindings (includes TCP/TLS interfaces)
        tracing::debug!("Adding sandbox bindings to linker");
        Sandbox::add_to_linker::<_, HasSelf<ExecutorState>>(&mut linker, |state| state)
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to add sandbox to linker: {e}")))?;
        tracing::debug!("Sandbox bindings added successfully");

        // External commands; only the shell guest imports this.
        crate::process::add_to_linker(&mut linker).map_err(|e| {
            ErrorKind::WasmEngine(format!("Failed to add eryx:process to linker: {e}"))
        })?;

        // Create pre-instantiated component
        // This validates that all imports are satisfied and prepares for fast instantiation
        let instance_pre = linker
            .instantiate_pre(component)
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to create instance_pre: {e}")))?;

        // Wrap in SandboxPre for typed access to exports
        SandboxPre::new(instance_pre)
            .map_err(|e| ErrorKind::WasmEngine(format!("Failed to create SandboxPre: {e}")).into())
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
        name = "Executor::execute_internal",
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
        output_tx: Option<mpsc::UnboundedSender<OutputRequest>>,
        memory_limit: Option<u64>,
        execution_timeout: Option<Duration>,
        cancellation_token: Option<CancellationToken>,
        fuel_limit: Option<u64>,
        max_child_host_bytes: Option<u64>,
        child_net: Option<crate::process::ChildNet>,
        allowed_commands: Option<&[String]>,
        command_options: &HashMap<String, crate::process::CommandOptions>,
        credentials: &[Arc<crate::net::Credential>],
        #[cfg(feature = "unstable-vfs")] vfs_storage: Option<eryx_vfs::ArcStorage>,
        #[cfg(feature = "unstable-vfs")] volumes: Vec<crate::session::VolumeMount>,
        #[cfg(feature = "unstable-vfs")] max_open_host_files: Option<usize>,
        #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
        vfs_limits: crate::session::executor::VfsLimits,
    ) -> std::result::Result<ExecutionOutput, Error> {
        crate::error::validate_user_code(code)?;
        // The execution's first: for a header both set on a request, the
        // first match wins (`inject_credentials`), so the execution's does.
        let credentials: Vec<_> = credentials
            .iter()
            .chain(&self.credentials)
            .cloned()
            .collect();
        let commands = self.commands.for_execution(
            allowed_commands,
            command_options,
            child_net.is_some(),
            &credentials,
        )?;

        // Avoid making an already-cancelled request wait for the first epoch
        // tick. Cancellation that races this check is still observed by the
        // Store-local deadline callback below.
        if is_already_cancelled(cancellation_token.as_ref()) {
            return Err(ErrorKind::Cancelled.into());
        }

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
        let fd_output = FdOutput::default();
        fd_output.attach(&mut wasi_builder);

        // Build PYTHONPATH from stdlib and all site-packages directories
        // The first site-packages is mounted at /site-packages (for preinit compatibility)
        // Additional ones are mounted at /site-packages-1, /site-packages-2, etc.
        let mut pythonpath_parts = Vec::new();
        if self.stdlib_path.is_some() {
            pythonpath_parts.push("/python-stdlib".to_string());
        }
        for i in 0..self.library_paths.len() {
            pythonpath_parts.push(site_packages_mount_path(i));
        }

        // Mount Python stdlib if configured (required for eryx-guest-python)
        if let Some(ref stdlib_path) = self.stdlib_path {
            // PYTHONHOME tells Python where to find the standard library
            wasi_builder.env("PYTHONHOME", "/python-stdlib");
            wasi_builder
                .preopened_dir(stdlib_path, "/python-stdlib", FsPerms::ReadOnly)
                .map_err(|e| {
                    ErrorKind::Initialization(format!("Failed to mount Python stdlib: {e}"))
                })?;
        }

        // Set PYTHONPATH for all configured paths (stdlib and/or site-packages)
        if !pythonpath_parts.is_empty() {
            wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
        }

        // Mount each site-packages directory
        // First one at /site-packages (for preinit compatibility), rest at /site-packages-N
        for (i, site_packages_path) in self.library_paths.iter().enumerate() {
            let mount_path = site_packages_mount_path(i);
            wasi_builder
                .preopened_dir(site_packages_path, &mount_path, FsPerms::ReadOnly)
                .map_err(|e| {
                    ErrorKind::Initialization(format!("Failed to mount {mount_path}: {e}"))
                })?;
        }

        #[cfg(feature = "environment")]
        if let Some(root) = &self.environment {
            wasi_builder
                .preopened_dir(root, crate::environment::MOUNT_PATH, FsPerms::ReadOnly)
                .map_err(|e| {
                    ErrorKind::Initialization(format!("Failed to mount the environment: {e}"))
                })?;
        }

        if self.language == Language::Shell {
            add_shell_env(&mut wasi_builder);
        }
        let wasi = wasi_builder.build();

        // Without `unstable-vfs`, the hybrid VFS is only linked for the
        // shell's filesystem: a private /tmp under the quota, /dev and the
        // root, and for Python the same mounts as above.
        #[cfg(all(feature = "shell", not(feature = "unstable-vfs")))]
        let hybrid_vfs_ctx = Some(crate::session::executor::build_private_vfs_context(
            self,
            crate::session::executor::private_vfs_storage(vfs_limits),
            None,
        )?);

        // Build hybrid VFS context when the `unstable-vfs` feature is enabled.
        // The hybrid VFS routes /data/* to VFS storage while passing through
        // other paths (like /python-stdlib/*) to the real filesystem.
        // When no VFS storage is provided, we still configure the hybrid VFS
        // with real filesystem preopens to satisfy the linker bindings.
        #[cfg(feature = "unstable-vfs")]
        let hybrid_vfs_ctx = {
            // Use provided storage or create a plain in-memory storage (no scrubbing)
            let storage = vfs_storage.unwrap_or_else(|| {
                eryx_vfs::ArcStorage::new(std::sync::Arc::new(eryx_vfs::InMemoryStorage::new()))
            });
            Some(crate::session::executor::build_hybrid_vfs_context(
                self,
                storage,
                &crate::session::VfsConfig::default().with_volumes(volumes),
                max_open_host_files,
            )?)
        };

        let state = ExecutorState {
            wasi,
            table: ResourceTable::new(),
            callback_tx,
            trace_tx,
            callbacks: callback_infos,
            memory_tracker: MemoryTracker::new(memory_limit),
            net_tx,
            output_tx,
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            hybrid_vfs_ctx,
            suspended: None,
            reuse_empty_callbacks: true,
            stream_budget: StreamBudget::default(),
            commands,
            child_budget: crate::process::ChildBudget::new(max_child_host_bytes),
            child_net: child_net.map(|net| std::sync::Arc::new(std::sync::Mutex::new(net))),
            trace_clock: TraceClock::default(),
        };

        // Set up fuel for tracking/limiting. We use u64::MAX for tracking-only mode
        // when no explicit limit is set. Fuel is consumed per WASM instruction.
        let initial_fuel = fuel_limit.unwrap_or(u64::MAX);

        let (mut store, bindings) = self
            .acquire_store(state, memory_limit, initial_fuel)
            .await?;

        let mut result = self
            .run_on_store(
                &mut store,
                &bindings,
                code,
                execution_timeout,
                cancellation_token,
                fuel_limit,
                initial_fuel,
            )
            .await;
        let (fd_stdout, fd_stderr) = fd_output.take(store.data().output_tx.as_ref());
        match &mut result {
            Ok(output) => {
                output.stdout.extend(fd_stdout);
                output.stderr.extend(fd_stderr);
            }
            // The traceback is the error's stderr.
            Err(e) => {
                if let ErrorKind::ScriptFailed {
                    message,
                    exit_code,
                    stdout,
                    stderr,
                } = e.kind_mut()
                {
                    append_fd_stderr(message, &fd_stderr, self.language, *exit_code);
                    stdout.extend(fd_stdout);
                    stderr.extend(fd_stderr);
                }
            }
        }
        // The fuel and memory a failed run used are still the host's to meter.
        let result = result.map_err(|e| {
            let remaining = store.get_fuel().unwrap_or(0);
            e.with_run_stats(
                Duration::ZERO, // Duration tracked by higher-level APIs (Sandbox, Session)
                store.data().memory_tracker.peak_memory_bytes(),
                Some(initial_fuel.saturating_sub(remaining)),
            )
            .with_output_truncated(store.data().stream_budget.output_truncated)
        });

        // Teardown (resetting the memory slot) and re-instantiation happen off
        // the request path when the warm pool is active. A failed execution
        // (timeout, cancellation, trap) may leave host calls suspended in the
        // store holding channel senders that the caller's handler tasks wait
        // on, so that store is dropped here, as it would be without the pool.
        let store = if result.is_ok() {
            Some(store)
        } else {
            drop(store);
            None
        };
        WarmPool::global().retire(&self.instance_pre, store);
        result
    }

    /// Instantiate the guest in `store` and run it as a command, with argv
    /// `args`, environment `env` and working directory `cwd`, through the
    /// `run-command` export; its exit status. The store's state comes from
    /// [`ExecutorState::for_command`], and its fuel and epoch deadline are the
    /// caller's to set.
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    pub(crate) async fn run_command(
        &self,
        store: &mut Store<ExecutorState>,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: String,
    ) -> wasmtime::Result<i32> {
        let bindings = self.instance_pre.instantiate_async(&mut *store).await?;
        store
            .run_concurrent(async move |accessor| {
                bindings.call_run_command(accessor, args, env, cwd).await
            })
            .await?
    }

    /// Take a warm store for this component, or instantiate a fresh one.
    ///
    /// A warm store was instantiated with a placeholder state; the real
    /// per-execution `state` replaces it here, before any export is called.
    async fn acquire_store(
        &self,
        state: ExecutorState,
        memory_limit: Option<u64>,
        initial_fuel: u64,
    ) -> std::result::Result<(Store<ExecutorState>, Sandbox), Error> {
        if let Some(WarmInstance {
            mut store,
            bindings,
        }) = WarmPool::global().take(&self.instance_pre, memory_limit)
        {
            // The placeholder's tracker saw the memory instantiation mapped;
            // carry that into this execution's peak.
            let baseline = store.data().memory_tracker.peak_memory_bytes();
            state.memory_tracker.observe(baseline);
            *store.data_mut() = state;
            store
                .set_fuel(initial_fuel)
                .map_err(|e| ErrorKind::Initialization(format!("Failed to set fuel: {e}")))?;
            return Ok((store, bindings));
        }

        instantiate_store(&self.instance_pre, state, initial_fuel).await
    }

    /// Run `code` on an instantiated store.
    #[allow(clippy::too_many_arguments)]
    async fn run_on_store(
        &self,
        store: &mut Store<ExecutorState>,
        bindings: &Sandbox,
        code: &str,
        execution_timeout: Option<Duration>,
        cancellation_token: Option<CancellationToken>,
        fuel_limit: Option<u64>,
        initial_fuel: u64,
    ) -> std::result::Result<ExecutionOutput, Error> {
        // Configure the guest's result-capture variable name. The guest defaults to
        // "result", so only call the export when a non-default name is configured.
        if self.result_variable != "result" {
            let name = self.result_variable.clone();
            store
                .run_concurrent(async |accessor| {
                    bindings.call_set_result_variable(accessor, name).await
                })
                .await
                .map_err(|e| ErrorKind::Execution(format!("set-result-variable failed: {e:?}")))?
                .map_err(ErrorKind::wasm_component)?;
        }

        tracing::debug!(code_len = code.len(), "Executing Python code");

        // Now set up epoch-based deadline for execution timeout and/or cancellation.
        // This is done AFTER instantiation so the timeout only applies to user code execution,
        // not Python initialization.
        // Track whether execution was cancelled (vs timed out)
        let was_cancelled = Arc::new(AtomicBool::new(false));

        // Untimed executions do not need the epoch clock. Once started, the single
        // process-lifetime ticker is shared by all timed and cancellable executions.
        if execution_timeout.is_some() || cancellation_token.is_some() {
            ensure_epoch_ticker(&self.engine)?;
        }

        if let Some(cancel_token) = cancellation_token.clone() {
            arm_epoch_cancellation(
                store,
                execution_timeout,
                cancel_token,
                Arc::clone(&was_cancelled),
            );
        } else {
            arm_epoch_deadline(store, execution_timeout);
        }

        // Call the async execute export
        let code_owned = code.to_string();

        // run_concurrent returns Result<R, Error> where R is the closure's return type.
        // Wrap in tokio::time::timeout so that blocking WASI host calls (e.g. poll_oneoff
        // used by time.sleep) are cancelled when the future is dropped, not just CPU-bound
        // loops caught by epoch interruption.
        //
        // Also race the cancellation token: the epoch callback only observes it
        // while guest code runs, so a guest suspended on a timer or callback
        // would otherwise ignore cancellation until that completes.
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
        let wasmtime_result = if let Some(timeout) = execution_timeout {
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

        // Classify errors using proper type matching. wasmtime::Error is anyhow::Error,
        // so we downcast to wasmtime::Trap for WASM-level traps (Interrupt, OutOfFuel).
        // The async_timeout_elapsed flag covers blocking WASI host calls (e.g. time.sleep).
        let wasmtime_result = wasmtime_result.map_err(|e| {
            // `was_cancelled` is set only by an interrupt or the select above.
            if async_timeout_elapsed
                || was_cancelled.load(Ordering::Relaxed)
                || e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
            {
                classify_epoch_interrupt(was_cancelled.load(Ordering::Relaxed), execution_timeout)
            } else if e.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
                // A callback that suspended poisons the store's fuel to halt the
                // guest, which surfaces as an OutOfFuel trap. Classify that as a
                // suspension rather than a real fuel-limit overrun (the suspended
                // flag is only set on the suspension path).
                if let Some(reason) = store.data().suspended.clone() {
                    ErrorKind::Suspended(reason).into()
                } else {
                    let remaining = store.get_fuel().unwrap_or(0);
                    let consumed = initial_fuel.saturating_sub(remaining);
                    let limit = fuel_limit.unwrap_or(u64::MAX);
                    ErrorKind::FuelExhausted { consumed, limit }.into()
                }
            } else {
                ErrorKind::Execution(format!("WASM execution error: {e:?}")).into()
            }
        })?;

        // wasmtime_result is Result<Result<ExecuteOutput, ExecuteError>, wasmtime::Error> from the WIT layer
        // The outer Result is from wasmtime, the inner is the WIT-generated result
        // where ExecuteOutput is the WIT-generated record with stdout and stderr
        // The inner WIT Err is the guest's logical execution error — for real
        // input that is always a script failure (for Python, its traceback).
        let wit_output = wasmtime_result
            .map_err(|e| ErrorKind::Execution(format!("WASM execution error: {e:?}")))?
            .map_err(|failure| ErrorKind::ScriptFailed {
                message: failure.message,
                exit_code: failure.exit_code,
                stdout: failure.stdout,
                stderr: failure.stderr,
            })?;

        // Get peak memory from the store before it's dropped
        let peak_memory_bytes = store.data().memory_tracker.peak_memory_bytes();

        // Calculate fuel consumed during execution
        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let fuel_consumed = Some(initial_fuel.saturating_sub(remaining_fuel));

        // Note: callback_invocations is 0 here because Executor doesn't
        // handle callbacks internally - it just passes the channel to the WASM state.
        // Higher-level APIs like Sandbox track callback invocations in their own
        // callback handler tasks.
        // The guest uses "" as the "absent" sentinel for both result fields
        // (a JSON value is never the empty string), so map empty -> None.
        let empty_to_none = |s: String| if s.is_empty() { None } else { Some(s) };

        let mut output = ExecutionOutput::new(
            wit_output.stdout,
            wit_output.stderr,
            peak_memory_bytes,
            Duration::ZERO, // Duration tracked by higher-level APIs (Sandbox, Session)
            0,              // Callback invocations tracked by higher-level APIs
            fuel_consumed,
        );
        output.result = empty_to_none(wit_output.result_json);
        output.result_error = empty_to_none(wit_output.result_error);
        output.stream_truncated = store.data().stream_budget.truncated;
        output.output_truncated = store.data().stream_budget.output_truncated;
        Ok(output)
    }
}

/// The most of a shell script's last stderr line its failure message quotes.
const SHELL_MESSAGE_LINE_MAX: usize = 1024;

/// Where to cut `line` at most at `end` without splitting a run of ASCII
/// letters, digits and underscores: secret placeholders are such runs
/// (`SecretConfig::with_placeholder`), and the message is scrubbed after
/// it's composed, so half of one would be left unredacted. The cut backs off
/// to the run's start, so the run is either whole (and scrubbed) or gone.
fn placeholder_safe_cut(line: &str, end: usize) -> usize {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = line.as_bytes();
    if !bytes.get(end).copied().is_some_and(word) {
        return end;
    }
    bytes[..end]
        .iter()
        .rposition(|&b| !word(b))
        .map_or(0, |i| i + 1)
}

/// Add the guest's direct fd 2 writes to a script failure's message, the way
/// a successful run appends them to stderr.
///
/// For Python and JavaScript the message is the exception (Python's
/// traceback), and fd 2 holds what the runtime itself printed around it: it's
/// appended on a line of its own. A shell failure's message is its exit
/// status, and fd 2 is the script's own stderr, already all in the failure's
/// `stderr`: the message quotes just its last line, as `exit status 127:
/// error: command not found: x`. A shell failure without an exit status (the
/// shell itself erred) keeps its message as it is: the stderr is unrelated.
pub(crate) fn append_fd_stderr(
    message: &mut String,
    fd_stderr: &[u8],
    language: Language,
    exit_code: Option<i32>,
) {
    let text = String::from_utf8_lossy(fd_stderr);
    if language == Language::Shell {
        if exit_code.is_none() {
            return;
        }
        if let Some(line) = text
            .lines()
            .map(str::trim_end)
            .rfind(|line| !line.is_empty())
        {
            message.push_str(": ");
            match line.char_indices().nth(SHELL_MESSAGE_LINE_MAX) {
                Some((end, _)) => {
                    message.push_str(&line[..placeholder_safe_cut(line, end)]);
                    message.push('…');
                }
                None => message.push_str(line),
            }
        }
        return;
    }
    if text.is_empty() {
        return;
    }
    if !message.is_empty() && !message.ends_with('\n') {
        message.push('\n');
    }
    message.push_str(&text);
}

/// Parse a trace request into a `TraceEvent`.
///
/// # Errors
///
/// Returns an error if the event JSON cannot be parsed.
pub fn parse_trace_event(request: &TraceRequest) -> std::result::Result<TraceEvent, Error> {
    let event_data: serde_json::Value = serde_json::from_str(&request.event_json)
        .map_err(|e| ErrorKind::Serialization(e.to_string()))?;

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
            // Added by the host; see `TraceClock`.
            let duration_ms = event_data
                .get("duration_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            crate::trace::TraceEventKind::CallbackEnd { name, duration_ms }
        }
        "process_start" | "process_end" => {
            let str_field = |key: &str| {
                event_data
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            };
            let name = str_field("name").unwrap_or_else(|| "<unknown>".to_owned());
            let pid = event_data
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .unwrap_or(0);
            if event_type == "process_start" {
                crate::trace::TraceEventKind::ProcessStart { name, pid }
            } else {
                crate::trace::TraceEventKind::ProcessEnd {
                    name,
                    pid,
                    exit_code: event_data
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .and_then(|code| i32::try_from(code).ok()),
                    duration_ms: event_data
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    error: str_field("error"),
                }
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

/// Ends a precompiled runtime stamped with its guest language, after the
/// language's snapshot tag. wasmtime ignores bytes after a component, so a
/// stamped file still loads anywhere a plain one does.
pub(crate) const LANGUAGE_STAMP_MAGIC: &[u8; 8] = b"ERYXLANG";

/// Ends a precompiled runtime stamped with its guest language and the hash
/// of its guest sources: `[hash u64 LE][language tag][magic]`.
const GUEST_STAMP_MAGIC: &[u8; 8] = b"ERYXLNG2";

/// The most bytes [`Executor::stamp_language`] appends.
#[cfg(any(feature = "embedded", feature = "preinit"))]
pub(crate) const LANGUAGE_STAMP_LEN: usize = 8 + 1 + GUEST_STAMP_MAGIC.len();

/// The hash of `language`'s guest sources this eryx was built with (see
/// `build/guest_sources.rs`), or `None` outside an eryx checkout.
///
/// It catches a stale runtime, not a tampered one: a 64-bit FNV-1a is
/// trivial to forge, and the stamp sits outside anything wasmtime checks.
/// Only load precompiled files you trust.
fn guest_sources_hash(language: Language) -> Option<u64> {
    let hex = match language {
        Language::Python => option_env!("ERYX_GUEST_HASH_PYTHON"),
        Language::JavaScript => option_env!("ERYX_GUEST_HASH_JAVASCRIPT"),
        Language::Shell => option_env!("ERYX_GUEST_HASH_SHELL"),
    }?;
    u64::from_str_radix(hex, 16).ok()
}

/// What [`Executor::stamp_language`] stamped a precompiled runtime with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanguageStamp {
    /// The guest language it runs.
    pub(crate) language: Language,
    /// The hash of the guest sources it was built from, if the eryx that
    /// stamped it was built from a checkout.
    guest_hash: Option<u64>,
}

impl LanguageStamp {
    /// Fail if the runtime was built from other guest sources than this eryx.
    /// Only checked when both have a hash.
    pub(crate) fn check_guest(self) -> std::result::Result<(), Error> {
        match (self.guest_hash, guest_sources_hash(self.language)) {
            (Some(found), Some(expected)) if found != expected => {
                let describe = |hash: u64| {
                    format!(
                        "{}, {} guest {hash:016x}",
                        Executor::module_version(),
                        self.language
                    )
                };
                Err(ErrorKind::IncompatiblePrecompiled {
                    built_by: describe(found),
                    expected: describe(expected),
                    guest: Some(self.language),
                }
                .into())
            }
            _ => Ok(()),
        }
    }
}

/// The stamp of the precompiled runtime in the first `len` bytes of `file`,
/// if any.
#[cfg(any(feature = "embedded", feature = "preinit"))]
pub(crate) fn file_language_stamp(file: &mut std::fs::File, len: u64) -> Option<LanguageStamp> {
    use std::io::{Read as _, Seek as _};

    // In a bundle the stamp ends the component, before the bundle's own
    // sections and footer.
    #[cfg(feature = "preinit")]
    let end = match crate::bundle::read_layout(file) {
        Ok(crate::bundle::Layout::Mapped { component_len, .. }) => component_len.min(len),
        _ => len,
    };
    #[cfg(not(feature = "preinit"))]
    let end = len;
    let mut tail = [0; LANGUAGE_STAMP_LEN];
    let start = end.checked_sub(tail.len() as u64)?;
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    file.read_exact(&mut tail).ok()?;
    language_stamp(&tail)
}

/// The stamp a precompiled runtime ending in `tail` got from
/// [`Executor::stamp_language`], or `None` if it wasn't stamped.
#[cfg(any(test, feature = "embedded", feature = "preinit"))]
pub(crate) fn language_stamp(tail: &[u8]) -> Option<LanguageStamp> {
    let (rest, magic) = tail.split_last_chunk::<8>()?;
    let (&tag, rest) = rest.split_last()?;
    let language = Language::from_snapshot_tag(tag)?;
    let guest_hash = if magic == LANGUAGE_STAMP_MAGIC {
        None
    } else if magic == GUEST_STAMP_MAGIC {
        Some(u64::from_le_bytes(*rest.split_last_chunk::<8>()?.1))
    } else {
        return None;
    };
    Some(LanguageStamp {
        language,
        guest_hash,
    })
}

/// Map an error from loading a precompiled component, reporting a file built
/// by another eryx version as [`ErrorKind::IncompatiblePrecompiled`].
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
pub(crate) fn deserialize_error(err: wasmtime::Error) -> Error {
    // wasmtime only reports these as text. If the wording ever changes,
    // callers just get the raw error again.
    const PREFIX: &str = "Module was compiled with incompatible version '";
    // The same eryx version with other engine settings (a development build
    // from before or after a wasm feature was switched on) passes the
    // version check, then fails this one.
    const FEATURE: &str = "Module was compiled with support for WebAssembly feature `";
    let message = format!("{err:#}");
    if let Some((_, rest)) = message.split_once(PREFIX)
        && let Some((built_by, _)) = rest.split_once('\'')
    {
        return ErrorKind::IncompatiblePrecompiled {
            built_by: built_by.to_owned(),
            expected: Executor::module_version(),
            guest: None,
        }
        .into();
    }
    if let Some((_, rest)) = message.split_once(FEATURE)
        && let Some((feature, _)) = rest.split_once('`')
    {
        return ErrorKind::IncompatiblePrecompiled {
            built_by: format!(
                "{} with other engine settings (it uses WebAssembly feature `{feature}`)",
                Executor::module_version()
            ),
            expected: Executor::module_version(),
            guest: None,
        }
        .into();
    }
    ErrorKind::wasm_component(err).into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    /// The callback clock times matched calls, keeps at most
    /// `MAX_PENDING_CALLBACKS` unmatched starts, and keeps none the trace
    /// budget dropped.
    #[test]
    fn the_trace_clock_is_bounded_and_charged() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = ExecutorState::placeholder();
        state.trace_tx = Some(tx);
        let event =
            |kind: &str, name: &str| serde_json::json!({ "type": kind, "name": name }).to_string();

        SandboxImports::report_trace(&mut state, 0, event("callback_start", "a"), String::new());
        SandboxImports::report_trace(&mut state, 0, event("callback_end", "a"), String::new());
        let _start = rx.try_recv().unwrap();
        let end: serde_json::Value =
            serde_json::from_str(&rx.try_recv().unwrap().event_json).unwrap();
        assert!(end["duration_ms"].is_u64(), "{end}");
        assert!(state.trace_clock.callbacks.is_empty());

        for i in 0..3 * MAX_PENDING_CALLBACKS {
            let name = format!("cb{i}");
            SandboxImports::report_trace(
                &mut state,
                0,
                event("callback_start", &name),
                String::new(),
            );
        }
        assert_eq!(state.trace_clock.callbacks.len(), MAX_PENDING_CALLBACKS);

        // Once the budget is spent, starts are dropped and not kept either.
        state.trace_clock = TraceClock::default();
        state.stream_budget.trace = 0;
        SandboxImports::report_trace(&mut state, 0, event("callback_start", "b"), String::new());
        assert!(state.trace_clock.callbacks.is_empty());
    }

    /// A child still running when its execution ends is reported killed even
    /// once the trace budget is spent, so its start isn't left unpaired; one
    /// whose start the budget dropped reports nothing.
    #[test]
    fn running_children_are_reported_killed_past_the_budget() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = ExecutorState::placeholder();
        state.trace_tx = Some(tx);
        let argv = ["tc".to_owned(), "spin".to_owned()];
        let _running = state.trace_process_start(&argv).unwrap();
        let start: serde_json::Value =
            serde_json::from_str(&rx.try_recv().unwrap().event_json).unwrap();
        assert_eq!(start["type"], "process_start");

        state.stream_budget.trace = 0;
        assert!(state.trace_process_start(&argv).is_none());
        state.end_running_children();
        let end: serde_json::Value =
            serde_json::from_str(&rx.try_recv().unwrap().event_json).unwrap();
        assert_eq!(
            (&end["type"], &end["pid"], &end["error"]),
            (&"process_end".into(), &1.into(), &"killed".into())
        );
        assert!(rx.try_recv().is_err());
    }

    /// A script failure's message gets the runtime's fd 2 writes on a line
    /// of their own, except a shell's: there they're the script's stderr,
    /// already in the failure's `stderr`, and its message quotes the last
    /// line after its exit status.
    #[test]
    fn fd_stderr_joins_the_message_on_its_own_line_or_the_shells_last_line() {
        let mut python = "Traceback (most recent call last):\n  ...\nValueError: x".to_string();
        append_fd_stderr(&mut python, b"warning from C\n", Language::Python, None);
        assert_eq!(
            python,
            "Traceback (most recent call last):\n  ...\nValueError: x\nwarning from C\n"
        );

        let mut ended = "Error: x\n".to_string();
        append_fd_stderr(&mut ended, b"more", Language::JavaScript, None);
        assert_eq!(ended, "Error: x\nmore");

        let mut nothing = "Error: x".to_string();
        append_fd_stderr(&mut nothing, b"", Language::JavaScript, None);
        assert_eq!(nothing, "Error: x");

        // The shell's message quotes the last non-empty line, capped.
        let mut shell = "exit status 127".to_string();
        append_fd_stderr(
            &mut shell,
            b"first\nerror: command not found: mcp.calc.add\n\n",
            Language::Shell,
            Some(127),
        );
        assert_eq!(
            shell,
            "exit status 127: error: command not found: mcp.calc.add"
        );
        let mut quiet = "exit status 3".to_string();
        append_fd_stderr(&mut quiet, b"", Language::Shell, Some(3));
        assert_eq!(quiet, "exit status 3");
        let mut long = "exit status 1".to_string();
        append_fd_stderr(
            &mut long,
            "é".repeat(5000).as_bytes(),
            Language::Shell,
            Some(1),
        );
        assert_eq!(
            long.chars().count(),
            "exit status 1: ".len() + SHELL_MESSAGE_LINE_MAX + 1
        );
        assert!(long.ends_with('…'));

        // The shell itself erred (no exit status): its message stands alone.
        let mut erred = "brush: something broke".to_string();
        append_fd_stderr(&mut erred, b"unrelated\n", Language::Shell, None);
        assert_eq!(erred, "brush: something broke");
    }

    /// The cap never splits a secret placeholder (a run of ASCII letters,
    /// digits and underscores), which scrubbing after it couldn't then
    /// match: a run straddling the cut is dropped whole, generated or custom.
    #[test]
    fn the_shell_message_cap_keeps_placeholders_whole() {
        for placeholder in [
            "ERYX_SECRET_PLACEHOLDER_0123456789abcdef0123456789abcdef",
            "my_own_placeholder_with_32_chars_or_more",
        ] {
            // A run of 1000 non-word characters, then the placeholder across
            // character 1024.
            let line = format!("{} {placeholder} tail", "-".repeat(1000));
            let mut message = "exit status 1".to_string();
            append_fd_stderr(&mut message, line.as_bytes(), Language::Shell, Some(1));
            assert_eq!(message, format!("exit status 1: {} …", "-".repeat(1000)));

            // Entirely before the cut it's kept whole, for scrubbing.
            let line = format!("{placeholder} {}", "-".repeat(2000));
            let mut message = "exit status 1".to_string();
            append_fd_stderr(&mut message, line.as_bytes(), Language::Shell, Some(1));
            assert!(message.contains(placeholder), "{message}");
        }
        // One run longer than the cap: nothing of it is quoted.
        let mut message = "exit status 1".to_string();
        append_fd_stderr(
            &mut message,
            "A".repeat(5000).as_bytes(),
            Language::Shell,
            Some(1),
        );
        assert_eq!(message, "exit status 1: …");
    }

    #[test]
    fn language_stamp_round_trips() {
        for language in [Language::Python, Language::JavaScript, Language::Shell] {
            let mut bytes = b"component".to_vec();
            Executor::stamp_language(&mut bytes, language);
            let stamp = language_stamp(&bytes).unwrap();
            assert_eq!(stamp.language, language);
            // Built from this checkout, so it carries this eryx's hash.
            assert_eq!(stamp.guest_hash, guest_sources_hash(language));
            assert!(stamp.guest_hash.is_some());
            stamp.check_guest().unwrap();
        }
        assert_eq!(language_stamp(b"component"), None);
        assert_eq!(language_stamp(b""), None);
        // An unknown tag isn't a language.
        assert_eq!(language_stamp(b"\x09ERYXLANG"), None);
        // A stamp from before guest hashes: the language, not checked.
        let old = language_stamp(b"component\x01ERYXLANG").unwrap();
        assert_eq!((old.language, old.guest_hash), (Language::JavaScript, None));
        old.check_guest().unwrap();
    }

    /// A runtime built from other guest sources is `IncompatiblePrecompiled`,
    /// naming the language and how to rebuild it.
    #[test]
    fn other_guest_sources_are_incompatible() {
        let mut bytes = b"component".to_vec();
        bytes.extend_from_slice(&0x1234_u64.to_le_bytes());
        bytes.push(Language::Shell.snapshot_tag());
        bytes.extend_from_slice(GUEST_STAMP_MAGIC);
        let err = language_stamp(&bytes).unwrap().check_guest().unwrap_err();
        assert!(matches!(
            err.kind(),
            ErrorKind::IncompatiblePrecompiled {
                guest: Some(Language::Shell),
                ..
            }
        ));
        let message = err.to_string();
        assert!(
            message.contains("shell guest 0000000000001234"),
            "{message}"
        );
        assert!(
            message.contains("mise run precompile-runtime-shell"),
            "{message}"
        );
    }

    use super::*;

    #[test]
    fn test_retryable_once_retries_after_failure() {
        let cell = RetryableOnce::new();

        let first = cell.get_or_try_init(|| Err::<u32, _>("transient failure"));
        assert_eq!(first.unwrap_err(), "transient failure");
        assert!(
            cell.value.get().is_none(),
            "failed initialization must not be cached"
        );

        let second = cell.get_or_try_init(|| Ok::<_, &str>(42)).unwrap();
        assert_eq!(*second, 42);

        let cached = cell
            .get_or_try_init(|| -> Result<u32, &str> {
                panic!("initializer must not run after success")
            })
            .unwrap();
        assert_eq!(*cached, 42);
    }

    #[test]
    fn test_epoch_ticker_dead_worker_is_reported_and_slot_is_retryable() {
        let mut ticker = Some(EpochTicker {
            handle: std::thread::spawn(|| {}),
            pid: std::process::id(),
        });
        while !ticker.as_ref().unwrap().handle.is_finished() {
            std::thread::yield_now();
        }

        let error = observe_epoch_ticker(&mut ticker).unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::EpochTicker(message) if message.contains("exited"))
        );
        assert!(ticker.is_none());

        // The cleared slot is ready for a later startup attempt. This short
        // worker keeps the test self-contained and does not spawn a permanent
        // ticker thread.
        ticker = Some(EpochTicker {
            handle: std::thread::spawn(|| {}),
            pid: std::process::id(),
        });
        while !ticker.as_ref().unwrap().handle.is_finished() {
            std::thread::yield_now();
        }
        assert!(observe_epoch_ticker(&mut ticker).is_err());
        assert!(ticker.is_none());
    }

    #[test]
    fn test_epoch_ticker_from_another_process_is_replaced() {
        // A live worker that "belongs" to another process, as after a fork.
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let mut ticker = Some(EpochTicker {
            handle: std::thread::spawn(move || {
                let _ = stopped.recv();
            }),
            pid: std::process::id().wrapping_add(1),
        });

        assert!(!observe_epoch_ticker(&mut ticker).unwrap());
        assert!(ticker.is_none(), "the inherited ticker must be dropped");
        drop(stop);
    }

    #[test]
    fn test_already_cancelled_token_is_detected_synchronously() {
        let token = CancellationToken::new();
        assert!(!is_already_cancelled(Some(&token)));
        token.cancel();
        assert!(is_already_cancelled(Some(&token)));
        assert!(!is_already_cancelled(None));
    }

    #[test]
    fn test_epoch_interrupt_classification_uses_captured_state() {
        let timeout = Duration::from_millis(50);
        let token = CancellationToken::new();
        let was_cancelled_at_interrupt = token.is_cancelled();
        token.cancel();

        assert!(matches!(
            classify_epoch_interrupt(was_cancelled_at_interrupt, Some(timeout)).kind(),
            ErrorKind::Timeout(duration) if *duration == timeout
        ));
        assert!(matches!(
            classify_epoch_interrupt(true, Some(timeout)).kind(),
            ErrorKind::Cancelled
        ));
    }

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

    fn allocator_settings(vars: &[(&str, &str)]) -> std::result::Result<AllocatorSettings, Error> {
        AllocatorSettings::from_lookup(|name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
    }

    #[test]
    fn allocator_defaults_to_pooling_except_on_windows() {
        let settings = allocator_settings(&[]).unwrap();
        assert_eq!(settings, AllocatorSettings::default());
        assert_eq!(settings.pooling_config().is_some(), !cfg!(windows));
    }

    #[test]
    fn allocator_can_be_switched_to_on_demand() {
        let settings = allocator_settings(&[("ERYX_ALLOCATOR", "On-Demand")]).unwrap();
        assert!(settings.pooling_config().is_none());
    }

    #[test]
    fn allocator_pool_limits_are_applied() {
        let settings = allocator_settings(&[
            ("ERYX_ALLOCATOR", "pooling"),
            ("ERYX_POOL_INSTANCES", " 42 "),
            ("ERYX_POOL_KEEP_RESIDENT_MB", "8"),
            ("ERYX_POOL_PAGEMAP_SCAN", "0"),
            ("ERYX_POOL_RELEASE_INTERVAL_SECS", "0"),
        ])
        .unwrap();
        assert_eq!(settings.release_interval, None);
        let pooling = settings.pooling_config().unwrap();
        assert_eq!(pooling.get_total_memories(), 42);
        assert_eq!(pooling.get_total_component_instances(), 42);
        assert_eq!(
            pooling.get_total_stacks(),
            42 * AllocatorSettings::STACKS_PER_SANDBOX
        );
        assert_eq!(pooling.get_memory_keep_resident(), 8 << 20);
        assert!(matches!(pooling.get_pagemap_scan(), Enabled::No));
    }

    #[test]
    fn allocator_rejects_invalid_values() {
        for vars in [
            [("ERYX_ALLOCATOR", "mmap")],
            [("ERYX_POOL_INSTANCES", "0")],
            [("ERYX_POOL_INSTANCES", "lots")],
            [("ERYX_POOL_KEEP_RESIDENT_MB", "-1")],
            [("ERYX_POOL_KEEP_RESIDENT_MB", "17592186044416")],
            [("ERYX_POOL_PAGEMAP_SCAN", "yes")],
            [("ERYX_POOL_RELEASE_INTERVAL_SECS", "soon")],
        ] {
            let err = allocator_settings(&vars).expect_err("invalid setting must be rejected");
            assert!(
                matches!(err.kind(), ErrorKind::WasmEngine(_)),
                "{vars:?}: {err:?}"
            );
        }
    }

    #[test]
    fn explicit_engine_config_beats_the_environment() {
        let env = |name: &str| match name {
            "ERYX_WARM_INSTANCES" => Some("5".to_string()),
            "ERYX_ALLOCATOR" => Some("pooling".to_string()),
            "ERYX_POOL_INSTANCES" => Some("7".to_string()),
            "ERYX_POOL_KEEP_RESIDENT_MB" => Some("16".to_string()),
            _ => None,
        };
        let config = SharedEngineConfig::default()
            .with_warm_instances(0)
            .with_allocator(InstanceAllocator::OnDemand)
            .with_pool_instances(3)
            .with_pool_pagemap_scan(false)
            .with_pool_release_interval_secs(0);
        assert_eq!(
            config.lookup("ERYX_WARM_INSTANCES", env).as_deref(),
            Some("0")
        );
        let settings = AllocatorSettings::from_lookup(|name| config.lookup(name, env)).unwrap();
        assert_eq!(
            settings,
            AllocatorSettings {
                pooling: false,
                pool_instances: 3,
                // Unset explicitly, so the environment's value applies.
                keep_resident_bytes: 16 << 20,
                pagemap_scan: false,
                release_interval: None,
            }
        );
    }

    #[test]
    fn explicit_engine_config_is_validated_like_the_environment() {
        let config = SharedEngineConfig::default().with_pool_instances(0);
        let err = AllocatorSettings::from_lookup(|name| config.lookup(name, |_| None))
            .expect_err("zero pool instances must be rejected");
        assert!(matches!(err.kind(), ErrorKind::WasmEngine(_)), "{err:?}");
    }

    /// The pooling configuration must be accepted by wasmtime. That
    /// `ERYX_POOL_INSTANCES` sandboxes fit in it is `tests/pool_capacity.rs`.
    #[test]
    #[cfg(feature = "embedded")]
    fn pooling_engine_is_constructible() {
        let mut config = Executor::base_engine_config().unwrap();
        let settings = allocator_settings(&[("ERYX_ALLOCATOR", "pooling")]).unwrap();
        let pooling = settings.pooling_config().unwrap();
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
        Engine::new(&config).expect("pooling configuration must be valid");
    }

    /// The engine keeps the host headroom on the fiber that
    /// `Executor::ASYNC_STACK_SIZE` requires.
    #[test]
    fn engine_leaves_host_stack_headroom() {
        let engine = Engine::new(&Executor::base_engine_config().unwrap()).unwrap();
        assert!(
            engine.get_async_stack_size() >= engine.get_max_wasm_stack() + Executor::MIN_HOST_STACK
        );
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
            let result = Executor::create_engine_with_target(None);
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

    /// Test that an explicit target survives `ERYX_CPU_FEATURES`.
    ///
    /// The preset used to re-pin the host triple, so cross-compiling with a
    /// CPU level set silently produced a host artifact.
    #[test]
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    fn test_cpu_feature_level_keeps_explicit_target() {
        use std::env;

        // An empty component: preamble only.
        const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
        let target = "x86_64-pc-windows-msvc";

        // SAFETY: Same as test_cpu_feature_levels_use_valid_cranelift_flags.
        unsafe {
            env::set_var("ERYX_CPU_FEATURES", "x86-64-v2");
            env::remove_var("ERYX_CRANELIFT_FLAGS");
            env::remove_var("ERYX_TARGET");
        }
        let result = Executor::precompile_with_target(EMPTY_COMPONENT, Some(target));
        // SAFETY: Same as above - single-threaded test cleanup.
        unsafe {
            env::remove_var("ERYX_CPU_FEATURES");
        }

        let bytes = result.expect("cross-compilation should succeed");
        assert!(
            bytes.windows(target.len()).any(|w| w == target.as_bytes()),
            "precompiled artifact should record the {target} triple"
        );
    }
}

/// The Python guest's `run-command` export, on the embedded runtime.
#[cfg(all(
    test,
    feature = "embedded",
    any(feature = "unstable-vfs", feature = "shell")
))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod run_command_tests {
    use super::*;
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

    struct Ran {
        status: i32,
        stdout: String,
        stderr: String,
    }

    /// Run `python3 ARGS` with `stdin`, in `cwd`, on a filesystem with a
    /// writable /tmp in `storage`.
    async fn run_in(
        storage: &eryx_vfs::ArcStorage,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: &str,
        stdin: &str,
    ) -> Ran {
        let executor = Executor::from_embedded_runtime().unwrap();
        let stdout = MemoryOutputPipe::new(1 << 20);
        let stderr = MemoryOutputPipe::new(1 << 20);
        let mut wasi = WasiCtxBuilder::new();
        wasi.stdin(MemoryInputPipe::new(stdin.to_owned()))
            .stdout(stdout.clone())
            .stderr(stderr.clone());
        let mut base = eryx_vfs::HybridVfsCtx::new(storage.clone());
        base.add_vfs_preopen(
            "/tmp",
            eryx_vfs::DirPerms::all(),
            eryx_vfs::FilePerms::all(),
        );
        let vfs = crate::session::executor::command_vfs_context(&executor, Some(base)).unwrap();
        let state = ExecutorState::for_command(wasi.build(), vfs, MemoryTracker::new(None));
        let mut store = Store::new(executor.engine(), state);
        store.limiter(|s| &mut s.memory_tracker);
        store.set_fuel(u64::MAX).unwrap();
        store.set_epoch_deadline(u64::MAX / 2);
        let args = std::iter::once("python3")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let status = executor
            .run_command(&mut store, args, env, cwd.to_owned())
            .await
            .unwrap();
        drop(store);
        Ran {
            status,
            stdout: String::from_utf8(stdout.contents().to_vec()).unwrap(),
            stderr: String::from_utf8(stderr.contents().to_vec()).unwrap(),
        }
    }

    async fn run(args: &[&str], stdin: &str) -> Ran {
        let storage = eryx_vfs::ArcStorage::new(Arc::new(eryx_vfs::InMemoryStorage::new()));
        run_in(&storage, args, &[], "/tmp", stdin).await
    }

    #[tokio::test]
    async fn dash_c_sees_its_argv() {
        let ran = run(&["-c", "import sys; print(sys.argv)", "a", "b"], "").await;
        assert_eq!(
            (ran.status, ran.stdout.as_str()),
            (0, "['-c', 'a', 'b']\n"),
            "{}",
            ran.stderr
        );
    }

    #[tokio::test]
    async fn code_and_data_come_from_stdin() {
        let ran = run(&[], "print(1 + 1)\n").await;
        assert_eq!(
            (ran.status, ran.stdout.as_str()),
            (0, "2\n"),
            "{}",
            ran.stderr
        );
        let ran = run(&["-", "x"], "import sys; print(sys.argv)").await;
        assert_eq!(ran.stdout, "['-', 'x']\n", "{}", ran.stderr);
        let ran = run(
            &["-c", "import sys; print(len(sys.stdin.read()))"],
            &"x".repeat(100_000),
        )
        .await;
        assert_eq!(ran.stdout, "100000\n", "{}", ran.stderr);
    }

    #[tokio::test]
    async fn exit_status_is_system_exit_or_1_with_a_traceback() {
        let ran = run(&["-c", "import sys; sys.exit(3)"], "").await;
        assert_eq!(ran.status, 3);
        let ran = run(&["-c", "raise SystemExit('bye')"], "").await;
        assert_eq!((ran.status, ran.stderr.as_str()), (1, "bye\n"));
        let ran = run(&["-c", "def f():\n    1 / 0\nf()"], "").await;
        assert_eq!(ran.status, 1);
        assert!(ran.stderr.starts_with("Traceback"), "{}", ran.stderr);
        assert!(ran.stderr.contains("ZeroDivisionError"), "{}", ran.stderr);
        assert!(!ran.stderr.contains("_eryx"), "eryx frames: {}", ran.stderr);
    }

    #[tokio::test]
    async fn usage_errors_exit_2() {
        let ran = run(&["-X", "dev"], "").await;
        assert_eq!(ran.status, 2, "{}", ran.stderr);
        assert!(
            ran.stderr.contains("unsupported option -X"),
            "{}",
            ran.stderr
        );
        let ran = run(&["-c"], "").await;
        assert_eq!(ran.status, 2, "{}", ran.stderr);
        let ran = run(&["/nope.py"], "").await;
        assert_eq!(ran.status, 2);
        assert!(
            ran.stderr.contains("can't open file '/nope.py'"),
            "{}",
            ran.stderr
        );
        let ran = run(&["-u", "-B", "-c", "print('flags ignored')"], "").await;
        assert_eq!(ran.stdout, "flags ignored\n", "{}", ran.stderr);
    }

    #[tokio::test]
    async fn env_and_cwd_are_the_callers() {
        let storage = eryx_vfs::ArcStorage::new(Arc::new(eryx_vfs::InMemoryStorage::new()));
        let ran = run_in(
            &storage,
            &[
                "-c",
                "import os; print(os.environ['FOO'], os.getcwd(), 'PATH' in os.environ)",
            ],
            &[("FOO", "bar")],
            "/tmp",
            "",
        )
        .await;
        assert_eq!(ran.stdout, "bar /tmp False\n", "{}", ran.stderr);
    }

    #[tokio::test]
    async fn modules_run_with_dash_m() {
        let ran = run(&["-m", "json.tool", "--compact"], r#"{"a": [1, 2]}"#).await;
        assert_eq!(
            (ran.status, ran.stdout.as_str()),
            (0, "{\"a\":[1,2]}\n"),
            "{}",
            ran.stderr
        );
    }

    #[tokio::test]
    async fn scripts_run_as_main_with_their_argv_and_asyncio() {
        let storage = eryx_vfs::ArcStorage::new(Arc::new(eryx_vfs::InMemoryStorage::new()));
        let script = "import asyncio, sys\n\
                      async def main():\n    await asyncio.sleep(0.01)\n    print(sys.argv, __name__, __file__)\n\
                      if __name__ == '__main__':\n    asyncio.run(main())\n    sys.exit(4)\n";
        let write = format!("open('/tmp/s.py', 'w').write({script:?})");
        let ran = run_in(&storage, &["-c", &write], &[], "/tmp", "").await;
        assert_eq!(ran.status, 0, "{}", ran.stderr);
        let ran = run_in(&storage, &["s.py", "x"], &[], "/tmp", "").await;
        assert_eq!(
            (ran.status, ran.stdout.as_str()),
            (4, "['s.py', 'x'] __main__ s.py\n"),
            "{}",
            ran.stderr
        );
    }
}
