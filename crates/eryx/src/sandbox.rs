//! Sandboxed Python execution environment.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;

use crate::callback::Callback;
use crate::error::Error;
use crate::library::RuntimeLibrary;
use crate::trace::{OutputHandler, TraceEvent, TraceHandler};
use crate::wasm::{CallbackRequest, PythonExecutor, TraceRequest, parse_trace_event};

/// A sandboxed Python execution environment.
pub struct Sandbox {
    /// The Python WASM executor (wrapped in Arc for sharing with sessions).
    executor: Arc<PythonExecutor>,
    /// Registered callbacks that Python code can invoke.
    callbacks: HashMap<String, Arc<dyn Callback>>,
    /// Python preamble code injected before user code.
    preamble: String,
    /// Combined type stubs from all libraries.
    type_stubs: String,
    /// Handler for execution trace events.
    trace_handler: Option<Arc<dyn TraceHandler>>,
    /// Handler for streaming stdout output.
    output_handler: Option<Arc<dyn OutputHandler>>,
    /// Resource limits for execution.
    resource_limits: ResourceLimits,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field(
                "callbacks",
                &format!("[{} callbacks]", self.callbacks.len()),
            )
            .field("preamble_len", &self.preamble.len())
            .field("type_stubs_len", &self.type_stubs.len())
            .field("has_trace_handler", &self.trace_handler.is_some())
            .field("has_output_handler", &self.output_handler.is_some())
            .field("resource_limits", &self.resource_limits)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    /// Create a sandbox builder.
    #[must_use]
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder::new()
    }

    /// Execute Python code in the sandbox.
    ///
    /// If an `OutputHandler` was configured, stdout is streamed to it during execution.
    /// If a `TraceHandler` was configured, trace events are emitted during execution.
    ///
    /// Returns the final result including complete stdout and collected trace events.
    ///
    /// # Errors
    ///
    /// Returns an error if the Python code fails to execute or a resource limit is exceeded.
    pub async fn execute(&self, code: &str) -> Result<ExecuteResult, Error> {
        let start = Instant::now();

        // Prepend preamble to user code if present
        let full_code = if self.preamble.is_empty() {
            code.to_string()
        } else {
            format!("{}\n\n# User code\n{}", self.preamble, code)
        };

        // Create channels for callback requests and trace events
        let (callback_tx, callback_rx) = mpsc::channel::<CallbackRequest>(32);
        let (trace_tx, trace_rx) = mpsc::unbounded_channel::<TraceRequest>();

        // Collect callbacks as a Vec for the executor
        let callbacks: Vec<Arc<dyn Callback>> = self.callbacks.values().cloned().collect();

        // Spawn task to handle callback requests concurrently
        let callbacks_map = self.callbacks.clone();
        let resource_limits = self.resource_limits.clone();
        let callback_handler = tokio::spawn(async move {
            Self::run_callback_handler(callback_rx, callbacks_map, resource_limits).await
        });

        // Spawn task to handle trace events
        let trace_handler = self.trace_handler.clone();
        let trace_collector =
            tokio::spawn(async move { Self::run_trace_collector(trace_rx, trace_handler).await });

        // Execute the Python code with optional timeout
        let memory_limit = self.resource_limits.max_memory_bytes;
        let execute_future = self.executor.execute(
            &full_code,
            &callbacks,
            Some(callback_tx),
            Some(trace_tx),
            memory_limit,
        );

        let execution_result = if let Some(timeout) = self.resource_limits.execution_timeout {
            tokio::time::timeout(timeout, execute_future)
                .await
                .unwrap_or_else(|_| Err(format!("Execution timed out after {timeout:?}")))
        } else {
            execute_future.await
        };

        // Wait for the handler tasks to complete
        // The callback channel is closed when execute_future completes (callback_tx dropped)
        let callback_invocations = callback_handler.await.unwrap_or(0);
        let trace_events = trace_collector.await.unwrap_or_default();

        let duration = start.elapsed();

        match execution_result {
            Ok(output) => {
                // Stream output if handler is configured
                if let Some(handler) = &self.output_handler {
                    handler.on_output(&output.stdout).await;
                }

                Ok(ExecuteResult {
                    stdout: output.stdout,
                    trace: trace_events,
                    stats: ExecuteStats {
                        duration,
                        callback_invocations,
                        peak_memory_bytes: Some(output.peak_memory_bytes),
                    },
                })
            }
            Err(error) => Err(Error::Execution(error)),
        }
    }

    /// Handle callback requests with concurrent execution.
    ///
    /// Uses `tokio::select!` to concurrently:
    /// 1. Receive new callback requests from the channel
    /// 2. Poll in-flight callback futures to completion
    ///
    /// This allows multiple callbacks to execute in parallel when Python code
    /// uses `asyncio.gather()` or similar patterns.
    async fn run_callback_handler(
        mut callback_rx: mpsc::Receiver<CallbackRequest>,
        callbacks_map: HashMap<String, Arc<dyn Callback>>,
        resource_limits: ResourceLimits,
    ) -> u32 {
        let invocation_count = Arc::new(AtomicU32::new(0));
        let mut in_flight: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        > = FuturesUnordered::new();

        loop {
            tokio::select! {
                // Receive new callback requests
                request = callback_rx.recv() => {
                    if let Some(req) = request {
                        if let Some(fut) = Self::create_callback_future(
                            req,
                            &callbacks_map,
                            &resource_limits,
                            &invocation_count,
                        ) {
                            in_flight.push(fut);
                        }
                    } else {
                        // Channel closed, drain remaining futures and exit
                        while in_flight.next().await.is_some() {}
                        break;
                    }
                }

                // Poll in-flight callbacks
                Some(()) = in_flight.next(), if !in_flight.is_empty() => {
                    // A callback completed, continue the loop
                }
            }
        }

        invocation_count.load(Ordering::SeqCst)
    }

    /// Create a future for executing a single callback.
    fn create_callback_future(
        request: CallbackRequest,
        callbacks_map: &HashMap<String, Arc<dyn Callback>>,
        resource_limits: &ResourceLimits,
        invocation_count: &Arc<AtomicU32>,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        // Check callback limit
        let current_count = invocation_count.fetch_add(1, Ordering::SeqCst);
        if let Some(max) = resource_limits.max_callback_invocations
            && current_count >= max
        {
            let _ = request
                .response_tx
                .send(Err(format!("Callback limit exceeded ({max} invocations)")));
            return None;
        }

        // Find the callback
        let Some(callback) = callbacks_map.get(&request.name).cloned() else {
            let _ = request
                .response_tx
                .send(Err(format!("Callback '{}' not found", request.name)));
            return None;
        };

        // Parse arguments
        let args: serde_json::Value = match serde_json::from_str(&request.arguments_json) {
            Ok(v) => v,
            Err(e) => {
                let _ = request
                    .response_tx
                    .send(Err(format!("Invalid arguments JSON: {e}")));
                return None;
            }
        };

        // Create the future
        let timeout = resource_limits.callback_timeout;
        let fut = async move {
            let invoke_future = callback.invoke(args);

            let callback_result = if let Some(timeout) = timeout {
                tokio::time::timeout(timeout, invoke_future)
                    .await
                    .map_or(Err(crate::callback::CallbackError::Timeout), |r| r)
            } else {
                invoke_future.await
            };

            let result = match callback_result {
                Ok(value) => Ok(value.to_string()),
                Err(e) => Err(e.to_string()),
            };

            // Send result back to the Python code
            let _ = request.response_tx.send(result);
        };

        Some(Box::pin(fut))
    }

    /// Collect trace events from the Python runtime.
    async fn run_trace_collector(
        mut trace_rx: mpsc::UnboundedReceiver<TraceRequest>,
        trace_handler: Option<Arc<dyn TraceHandler>>,
    ) -> Vec<TraceEvent> {
        let mut events = Vec::new();

        while let Some(request) = trace_rx.recv().await {
            if let Ok(event) = parse_trace_event(&request) {
                // Send to trace handler if configured
                if let Some(handler) = &trace_handler {
                    handler.on_trace(event.clone()).await;
                }
                events.push(event);
            }
        }

        events
    }

    /// Get combined type stubs for all loaded libraries.
    /// Useful for including in LLM context windows.
    #[must_use]
    pub fn type_stubs(&self) -> &str {
        &self.type_stubs
    }

    /// Get a reference to the registered callbacks.
    #[must_use]
    pub fn callbacks(&self) -> &HashMap<String, Arc<dyn Callback>> {
        &self.callbacks
    }

    /// Get the Python preamble code.
    #[must_use]
    pub fn preamble(&self) -> &str {
        &self.preamble
    }

    /// Get a reference to the trace handler.
    #[must_use]
    pub fn trace_handler(&self) -> &Option<Arc<dyn TraceHandler>> {
        &self.trace_handler
    }

    /// Get a reference to the output handler.
    #[must_use]
    pub fn output_handler(&self) -> &Option<Arc<dyn OutputHandler>> {
        &self.output_handler
    }

    /// Get a reference to the resource limits.
    #[must_use]
    pub fn resource_limits(&self) -> &ResourceLimits {
        &self.resource_limits
    }

    /// Get a reference to the Python executor.
    ///
    /// This is primarily for internal use by session implementations.
    #[must_use]
    pub(crate) fn executor(&self) -> Arc<PythonExecutor> {
        self.executor.clone()
    }
}

/// Source of the WASM component for the sandbox.
#[derive(Debug, Clone)]
enum WasmSource {
    /// No source specified yet.
    None,
    /// WASM component bytes (will be compiled at load time).
    Bytes(Vec<u8>),
    /// Path to a WASM component file (will be compiled at load time).
    File(std::path::PathBuf),
    /// Pre-compiled component bytes (skip compilation, unsafe).
    #[cfg(feature = "precompiled")]
    PrecompiledBytes(Vec<u8>),
    /// Path to a pre-compiled component file (skip compilation, unsafe).
    #[cfg(feature = "precompiled")]
    PrecompiledFile(std::path::PathBuf),
    /// Use the embedded pre-compiled runtime (safe, fast).
    #[cfg(feature = "embedded-runtime")]
    EmbeddedRuntime,
}

impl Default for WasmSource {
    fn default() -> Self {
        Self::None
    }
}

/// Builder for constructing a [`Sandbox`].
pub struct SandboxBuilder {
    wasm_source: WasmSource,
    callbacks: HashMap<String, Arc<dyn Callback>>,
    preamble: String,
    type_stubs: String,
    trace_handler: Option<Arc<dyn TraceHandler>>,
    output_handler: Option<Arc<dyn OutputHandler>>,
    resource_limits: ResourceLimits,
}

impl Default for SandboxBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SandboxBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxBuilder")
            .field(
                "callbacks",
                &format!("[{} callbacks]", self.callbacks.len()),
            )
            .field("preamble_len", &self.preamble.len())
            .field("type_stubs_len", &self.type_stubs.len())
            .field("has_trace_handler", &self.trace_handler.is_some())
            .field("has_output_handler", &self.output_handler.is_some())
            .field("resource_limits", &self.resource_limits)
            .field("wasm_source", &self.wasm_source)
            .finish()
    }
}

impl SandboxBuilder {
    /// Create a new sandbox builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            wasm_source: WasmSource::None,
            callbacks: HashMap::new(),
            preamble: String::new(),
            type_stubs: String::new(),
            trace_handler: None,
            output_handler: None,
            resource_limits: ResourceLimits::default(),
        }
    }

    /// Use the embedded pre-compiled runtime for ~50x faster sandbox creation.
    ///
    /// This is the recommended way to create sandboxes for production use.
    /// The runtime is pre-compiled at build time when the `embedded-runtime`
    /// feature is enabled.
    ///
    /// # Panics
    ///
    /// Panics at build time if the `embedded-runtime` feature is enabled but
    /// `runtime.wasm` is not found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Fast and safe - no unsafe code needed!
    /// let sandbox = Sandbox::builder()
    ///     .with_embedded_runtime()
    ///     .build()?;
    /// ```
    #[cfg(feature = "embedded-runtime")]
    #[must_use]
    pub fn with_embedded_runtime(mut self) -> Self {
        self.wasm_source = WasmSource::EmbeddedRuntime;
        self
    }

    /// Set the WASM component from bytes.
    ///
    /// Use this to embed the WASM component in your binary.
    #[must_use]
    pub fn with_wasm_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.wasm_source = WasmSource::Bytes(bytes.into());
        self
    }

    /// Set the WASM component from a file path.
    #[must_use]
    pub fn with_wasm_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.wasm_source = WasmSource::File(path.into());
        self
    }

    /// Set the WASM component from pre-compiled bytes.
    ///
    /// Pre-compiled components load much faster because they skip compilation
    /// (~50x faster sandbox creation). Create pre-compiled bytes using
    /// `PythonExecutor::precompile()`.
    ///
    /// # Safety
    ///
    /// This function is unsafe because wasmtime cannot fully validate
    /// pre-compiled components for safety. Loading untrusted pre-compiled
    /// bytes can lead to **arbitrary code execution**.
    ///
    /// Only call this with pre-compiled bytes that:
    /// - Were created by `PythonExecutor::precompile()` or `precompile_file()`
    /// - Come from a trusted source you control
    /// - Were compiled with a compatible wasmtime version and configuration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Pre-compile once (safe operation)
    /// let precompiled = PythonExecutor::precompile_file("runtime.wasm")?;
    ///
    /// // Load from pre-compiled (unsafe - you must trust the bytes)
    /// let sandbox = unsafe {
    ///     Sandbox::builder()
    ///         .with_precompiled_bytes(precompiled)
    ///         .build()?
    /// };
    /// ```
    #[cfg(feature = "precompiled")]
    #[must_use]
    #[allow(unsafe_code)]
    pub unsafe fn with_precompiled_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.wasm_source = WasmSource::PrecompiledBytes(bytes.into());
        self
    }

    /// Set the WASM component from a pre-compiled file path.
    ///
    /// Pre-compiled components load much faster because they skip compilation
    /// (~50x faster sandbox creation). Create pre-compiled files using
    /// `PythonExecutor::precompile_file()`.
    ///
    /// # Safety
    ///
    /// This function is unsafe because wasmtime cannot fully validate
    /// pre-compiled components for safety. Loading untrusted pre-compiled
    /// files can lead to **arbitrary code execution**.
    ///
    /// Only call this with pre-compiled files that:
    /// - Were created by `PythonExecutor::precompile()` or `precompile_file()`
    /// - Come from a trusted source you control
    /// - Were compiled with a compatible wasmtime version and configuration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Pre-compile once and save to disk
    /// let precompiled = PythonExecutor::precompile_file("runtime.wasm")?;
    /// std::fs::write("runtime.cwasm", &precompiled)?;
    ///
    /// // Load from pre-compiled file (unsafe - you must trust the file)
    /// let sandbox = unsafe {
    ///     Sandbox::builder()
    ///         .with_precompiled_file("runtime.cwasm")
    ///         .build()?
    /// };
    /// ```
    #[cfg(feature = "precompiled")]
    #[must_use]
    #[allow(unsafe_code)]
    pub unsafe fn with_precompiled_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.wasm_source = WasmSource::PrecompiledFile(path.into());
        self
    }

    /// Add a runtime library (callbacks + preamble + stubs).
    #[must_use]
    pub fn with_library(mut self, library: RuntimeLibrary) -> Self {
        // Add callbacks from the library
        for callback in library.callbacks {
            self.callbacks
                .insert(callback.name().to_string(), Arc::from(callback));
        }

        // Append preamble
        if !library.python_preamble.is_empty() {
            if !self.preamble.is_empty() {
                self.preamble.push('\n');
            }
            self.preamble.push_str(&library.python_preamble);
        }

        // Append type stubs
        if !library.type_stubs.is_empty() {
            if !self.type_stubs.is_empty() {
                self.type_stubs.push('\n');
            }
            self.type_stubs.push_str(&library.type_stubs);
        }

        self
    }

    /// Add individual callbacks.
    #[must_use]
    pub fn with_callbacks(mut self, callbacks: Vec<Box<dyn Callback>>) -> Self {
        for callback in callbacks {
            self.callbacks
                .insert(callback.name().to_string(), Arc::from(callback));
        }
        self
    }

    /// Add a single callback.
    #[must_use]
    pub fn with_callback(mut self, callback: impl Callback + 'static) -> Self {
        let boxed: Box<dyn Callback> = Box::new(callback);
        self.callbacks
            .insert(boxed.name().to_string(), Arc::from(boxed));
        self
    }

    /// Set a trace handler for execution progress.
    #[must_use]
    pub fn with_trace_handler<H: TraceHandler + 'static>(mut self, handler: H) -> Self {
        self.trace_handler = Some(Arc::new(handler));
        self
    }

    /// Set an output handler for streaming stdout.
    #[must_use]
    pub fn with_output_handler<H: OutputHandler + 'static>(mut self, handler: H) -> Self {
        self.output_handler = Some(Arc::new(handler));
        self
    }

    /// Set resource limits.
    #[must_use]
    pub const fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Build the sandbox.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No WASM component was specified (use `with_embedded_runtime()`, `with_wasm_bytes()`, or `with_wasm_file()`)
    /// - The WASM component cannot be loaded
    /// - The WebAssembly runtime fails to initialize
    pub fn build(self) -> Result<Sandbox, Error> {
        let executor = match self.wasm_source {
            WasmSource::Bytes(bytes) => PythonExecutor::from_binary(&bytes)?,
            WasmSource::File(path) => PythonExecutor::from_file(&path)?,

            #[cfg(feature = "precompiled")]
            WasmSource::PrecompiledBytes(bytes) => {
                // SAFETY: User is responsible for only using trusted pre-compiled bytes.
                // The `with_precompiled_bytes` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled(&bytes)?
                }
            }

            #[cfg(feature = "precompiled")]
            WasmSource::PrecompiledFile(path) => {
                // SAFETY: User is responsible for only using trusted pre-compiled files.
                // The `with_precompiled_file` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled_file(&path)?
                }
            }

            #[cfg(feature = "embedded-runtime")]
            WasmSource::EmbeddedRuntime => {
                // SAFETY: The embedded runtime was pre-compiled at build time from our own
                // trusted runtime.wasm, so we know it's safe to deserialize.
                const EMBEDDED_RUNTIME: &[u8] =
                    include_bytes!(concat!(env!("OUT_DIR"), "/runtime.cwasm"));
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled(EMBEDDED_RUNTIME)?
                }
            }

            WasmSource::None => {
                #[cfg(feature = "embedded-runtime")]
                let msg = "No WASM component specified. Use with_embedded_runtime(), with_wasm_bytes(), with_wasm_file(), with_precompiled_bytes(), or with_precompiled_file().";

                #[cfg(all(feature = "precompiled", not(feature = "embedded-runtime")))]
                let msg = "No WASM component specified. Use with_wasm_bytes(), with_wasm_file(), with_precompiled_bytes(), or with_precompiled_file(). \
                           Or enable the `embedded-runtime` feature and use with_embedded_runtime().";

                #[cfg(not(feature = "precompiled"))]
                let msg = "No WASM component specified. Use with_wasm_bytes() or with_wasm_file(). \
                           Or enable the `precompiled` or `embedded-runtime` feature for faster loading options.";

                return Err(Error::Initialization(msg.to_string()));
            }
        };

        Ok(Sandbox {
            executor: Arc::new(executor),
            callbacks: self.callbacks,
            preamble: self.preamble,
            type_stubs: self.type_stubs,
            trace_handler: self.trace_handler,
            output_handler: self.output_handler,
            resource_limits: self.resource_limits,
        })
    }
}

/// Result of executing Python code in the sandbox.
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    /// Complete stdout output (also streamed via `OutputHandler` if configured).
    pub stdout: String,
    /// Collected trace events (also streamed via `TraceHandler` if configured).
    pub trace: Vec<TraceEvent>,
    /// Execution statistics.
    pub stats: ExecuteStats,
}

/// Statistics about sandbox execution.
#[derive(Debug, Clone)]
pub struct ExecuteStats {
    /// Total execution time.
    pub duration: Duration,
    /// Number of callback invocations.
    pub callback_invocations: u32,
    /// Peak memory usage in bytes (if available).
    pub peak_memory_bytes: Option<u64>,
}

/// Resource limits for sandbox execution.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum execution time for the entire script.
    pub execution_timeout: Option<Duration>,
    /// Maximum time for a single callback invocation.
    pub callback_timeout: Option<Duration>,
    /// Maximum memory usage in bytes.
    pub max_memory_bytes: Option<u64>,
    /// Maximum number of callback invocations.
    pub max_callback_invocations: Option<u32>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            execution_timeout: Some(Duration::from_secs(30)),
            callback_timeout: Some(Duration::from_secs(10)),
            max_memory_bytes: Some(256 * 1024 * 1024), // 256 MB
            max_callback_invocations: Some(1000),
        }
    }
}
