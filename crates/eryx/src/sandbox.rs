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
    /// The Python WASM executor.
    executor: PythonExecutor,
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
        let execute_future =
            self.executor
                .execute(&full_code, &callbacks, Some(callback_tx), Some(trace_tx));

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
            Ok(stdout) => {
                // Stream output if handler is configured
                if let Some(handler) = &self.output_handler {
                    handler.on_output(&stdout).await;
                }

                Ok(ExecuteResult {
                    stdout,
                    trace: trace_events,
                    stats: ExecuteStats {
                        duration,
                        callback_invocations,
                        peak_memory_bytes: None, // TODO: Track memory usage
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
        if let Some(max) = resource_limits.max_callback_invocations {
            if current_count >= max {
                let _ = request
                    .response_tx
                    .send(Err(format!("Callback limit exceeded ({max} invocations)")));
                return None;
            }
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
}

/// Builder for constructing a [`Sandbox`].
pub struct SandboxBuilder {
    wasm_bytes: Option<Vec<u8>>,
    wasm_path: Option<std::path::PathBuf>,
    precompiled_bytes: Option<Vec<u8>>,
    precompiled_path: Option<std::path::PathBuf>,
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
            .field("has_wasm_bytes", &self.wasm_bytes.is_some())
            .field("wasm_path", &self.wasm_path)
            .finish()
    }
}

impl SandboxBuilder {
    /// Create a new sandbox builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            wasm_bytes: None,
            wasm_path: None,
            precompiled_bytes: None,
            precompiled_path: None,
            callbacks: HashMap::new(),
            preamble: String::new(),
            type_stubs: String::new(),
            trace_handler: None,
            output_handler: None,
            resource_limits: ResourceLimits::default(),
        }
    }

    /// Set the WASM component from bytes.
    ///
    /// Use this to embed the WASM component in your binary.
    #[must_use]
    pub fn with_wasm_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.wasm_bytes = Some(bytes.into());
        self.wasm_path = None;
        self.precompiled_bytes = None;
        self.precompiled_path = None;
        self
    }

    /// Set the WASM component from a file path.
    #[must_use]
    pub fn with_wasm_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.wasm_path = Some(path.into());
        self.wasm_bytes = None;
        self.precompiled_bytes = None;
        self.precompiled_path = None;
        self
    }

    /// Set the WASM component from pre-compiled bytes.
    ///
    /// Pre-compiled components load much faster because they skip compilation.
    /// Create pre-compiled bytes using `PythonExecutor::precompile()`.
    ///
    /// # Safety
    ///
    /// This is safe to call, but the resulting `build()` will use unsafe
    /// deserialization internally. Only use pre-compiled bytes you control
    /// and trust.
    #[must_use]
    pub fn with_precompiled_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.precompiled_bytes = Some(bytes.into());
        self.precompiled_path = None;
        self.wasm_bytes = None;
        self.wasm_path = None;
        self
    }

    /// Set the WASM component from a pre-compiled file path.
    ///
    /// Pre-compiled components load much faster because they skip compilation.
    /// Create pre-compiled files using `PythonExecutor::precompile_file()`.
    ///
    /// # Safety
    ///
    /// This is safe to call, but the resulting `build()` will use unsafe
    /// deserialization internally. Only use pre-compiled files you control
    /// and trust.
    #[must_use]
    pub fn with_precompiled_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.precompiled_path = Some(path.into());
        self.precompiled_bytes = None;
        self.wasm_bytes = None;
        self.wasm_path = None;
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
    /// - No WASM component was specified (use `with_wasm_bytes` or `with_wasm_file`)
    /// - The WASM component cannot be loaded
    /// - The WebAssembly runtime fails to initialize
    pub fn build(self) -> Result<Sandbox, Error> {
        let executor = if let Some(bytes) = self.precompiled_bytes {
            #[allow(unsafe_code)]
            // Safety: User is responsible for only using trusted pre-compiled bytes
            unsafe {
                PythonExecutor::from_precompiled(&bytes)?
            }
        } else if let Some(path) = self.precompiled_path {
            // Safety: User is responsible for only using trusted pre-compiled files
            #[allow(unsafe_code)]
            unsafe {
                PythonExecutor::from_precompiled_file(&path)?
            }
        } else if let Some(bytes) = self.wasm_bytes {
            PythonExecutor::from_binary(&bytes)?
        } else if let Some(path) = self.wasm_path {
            PythonExecutor::from_file(&path)?
        } else {
            return Err(Error::Initialization(
                "No WASM component specified. Use with_wasm_bytes(), with_wasm_file(), with_precompiled_bytes(), or with_precompiled_file()."
                    .to_string(),
            ));
        };

        Ok(Sandbox {
            executor,
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
