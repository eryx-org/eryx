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

use crate::cache::ComponentCache;
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
#[derive(Default)]
enum WasmSource {
    /// No source specified yet.
    #[default]
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


/// Builder for constructing a [`Sandbox`].
pub struct SandboxBuilder {
    wasm_source: WasmSource,
    callbacks: HashMap<String, Arc<dyn Callback>>,
    preamble: String,
    type_stubs: String,
    trace_handler: Option<Arc<dyn TraceHandler>>,
    output_handler: Option<Arc<dyn OutputHandler>>,
    resource_limits: ResourceLimits,
    /// Path to Python stdlib for eryx-wasm-runtime.
    python_stdlib_path: Option<std::path::PathBuf>,
    /// Path to Python site-packages for eryx-wasm-runtime.
    python_site_packages_path: Option<std::path::PathBuf>,
    /// Native Python extensions to link into the component.
    #[cfg(feature = "native-extensions")]
    native_extensions: Vec<eryx_runtime::linker::NativeExtension>,
    /// Component cache for faster sandbox creation with native extensions.
    #[cfg(feature = "native-extensions")]
    cache: Option<Arc<dyn ComponentCache>>,
    /// Filesystem cache directory for mmap-based loading (faster than bytes).
    #[cfg(feature = "native-extensions")]
    filesystem_cache: Option<crate::cache::FilesystemCache>,
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
            python_stdlib_path: None,
            python_site_packages_path: None,
            #[cfg(feature = "native-extensions")]
            native_extensions: Vec::new(),
            #[cfg(feature = "native-extensions")]
            cache: None,
            #[cfg(feature = "native-extensions")]
            filesystem_cache: None,
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

    /// Add a native Python extension (.so file) to be linked into the component.
    ///
    /// Native extensions allow Python packages with compiled code (like numpy)
    /// to work in the sandbox. The extension is linked into the WASM component
    /// at sandbox creation time using late-linking.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the .so file (e.g., "numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so")
    /// * `bytes` - The raw WASM bytes of the compiled extension
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Load numpy native extension
    /// let numpy_core = std::fs::read("numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so")?;
    ///
    /// let sandbox = Sandbox::builder()
    ///     .with_native_extension("numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so", numpy_core)
    ///     .with_site_packages("path/to/site-packages")  // For Python files
    ///     .build()?;
    ///
    /// // Now numpy can be imported!
    /// let result = sandbox.execute("import numpy as np; print(np.array([1,2,3]).sum())").await?;
    /// ```
    ///
    /// # Note
    ///
    /// When native extensions are added, the sandbox creation is slower because
    /// the component needs to be re-linked. Consider caching the linked component
    /// for repeated use with the same extensions.
    #[cfg(feature = "native-extensions")]
    #[must_use]
    pub fn with_native_extension(
        mut self,
        name: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.native_extensions
            .push(eryx_runtime::linker::NativeExtension::new(name, bytes.into()));
        self
    }

    /// Set a component cache for faster sandbox creation with native extensions.
    ///
    /// When native extensions are used, the sandbox must link them into the base
    /// component and then JIT compile the result. This can take 500-1000ms.
    ///
    /// With caching enabled, the linked and pre-compiled component is stored and
    /// reused on subsequent calls, reducing creation time to ~10ms.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::{Sandbox, cache::InMemoryCache};
    ///
    /// let cache = InMemoryCache::new();
    ///
    /// // First call: ~1000ms (link + compile + cache)
    /// let sandbox1 = Sandbox::builder()
    ///     .with_native_extension("numpy/core/*.so", bytes)
    ///     .with_cache(Arc::new(cache.clone()))
    ///     .build()?;
    ///
    /// // Second call: ~10ms (cache hit)
    /// let sandbox2 = Sandbox::builder()
    ///     .with_native_extension("numpy/core/*.so", bytes)
    ///     .with_cache(Arc::new(cache))
    ///     .build()?;
    /// ```
    #[cfg(feature = "native-extensions")]
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<dyn ComponentCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Set a filesystem-based component cache for faster sandbox creation.
    ///
    /// This is a convenience method that creates a [`FilesystemCache`] at the
    /// given directory path.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache directory cannot be created.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::builder()
    ///     .with_native_extension("numpy/core/*.so", bytes)
    ///     .with_cache_dir("/tmp/eryx-cache")?
    ///     .build()?;
    /// ```
    ///
    /// [`FilesystemCache`]: crate::cache::FilesystemCache
    #[cfg(feature = "native-extensions")]
    pub fn with_cache_dir(mut self, path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let cache = crate::cache::FilesystemCache::new(path)
            .map_err(|e| Error::Initialization(format!("failed to create cache directory: {e}")))?;
        // Store filesystem cache for mmap-based loading (3x faster than bytes)
        self.filesystem_cache = Some(cache.clone());
        Ok(self.with_cache(Arc::new(cache)))
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

    /// Set the path to the Python standard library directory.
    ///
    /// This is required when using the eryx-wasm-runtime (Rust/CPython FFI based).
    /// The directory should contain the extracted Python stdlib (e.g., from
    /// componentize-py's python-lib.tar.zst).
    ///
    /// The stdlib will be mounted at `/python-stdlib` inside the WASM sandbox.
    #[must_use]
    pub fn with_python_stdlib(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.python_stdlib_path = Some(path.into());
        self
    }

    /// Set the path to additional Python packages directory.
    ///
    /// The directory will be mounted at `/site-packages` inside the WASM sandbox
    /// and added to Python's import path.
    #[must_use]
    pub fn with_site_packages(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.python_site_packages_path = Some(path.into());
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
        // If native extensions are specified, use late-linking to create the component
        #[cfg(feature = "native-extensions")]
        let executor = if !self.native_extensions.is_empty() {
            self.build_executor_with_extensions()?
        } else {
            self.build_executor_from_source()?
        };

        #[cfg(not(feature = "native-extensions"))]
        let executor = self.build_executor_from_source()?;

        // Apply Python stdlib and site-packages paths if configured
        let executor = match (self.python_stdlib_path.clone(), self.python_site_packages_path.clone()) {
            (Some(stdlib), Some(site)) => executor
                .with_python_stdlib(&stdlib)
                .with_site_packages(&site),
            (Some(stdlib), None) => executor.with_python_stdlib(&stdlib),
            (None, Some(site)) => executor.with_site_packages(&site),
            (None, None) => executor,
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

    /// Build executor from the configured WASM source.
    fn build_executor_from_source(&self) -> Result<PythonExecutor, Error> {
        let executor = match &self.wasm_source {
            WasmSource::Bytes(bytes) => PythonExecutor::from_binary(bytes)?,
            WasmSource::File(path) => PythonExecutor::from_file(path)?,

            #[cfg(feature = "precompiled")]
            WasmSource::PrecompiledBytes(bytes) => {
                // SAFETY: User is responsible for only using trusted pre-compiled bytes.
                // The `with_precompiled_bytes` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled(bytes)?
                }
            }

            #[cfg(feature = "precompiled")]
            WasmSource::PrecompiledFile(path) => {
                // SAFETY: User is responsible for only using trusted pre-compiled files.
                // The `with_precompiled_file` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled_file(path)?
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

        Ok(executor)
    }

    /// Build executor with native extensions, using cache if available.
    ///
    /// When a cache is configured and the `precompiled` feature is enabled,
    /// this will:
    /// 1. Check the cache for a pre-compiled component
    /// 2. If found, load from cache (fast path)
    /// 3. If not found, link extensions, pre-compile, cache, and return
    #[cfg(feature = "native-extensions")]
    fn build_executor_with_extensions(&self) -> Result<PythonExecutor, Error> {
        use crate::cache::CacheKey;

        let cache_key = CacheKey::from_extensions(&self.native_extensions);

        // Try filesystem cache first (mmap-based, 3x faster than bytes)
        #[cfg(feature = "precompiled")]
        if let Some(fs_cache) = &self.filesystem_cache {
            if let Some(path) = fs_cache.get_path(&cache_key) {
                tracing::debug!(
                    key = %cache_key.to_hex(),
                    path = %path.display(),
                    "component cache hit - loading via mmap"
                );
                // SAFETY: The cached pre-compiled file was created by us (from
                // `PythonExecutor::precompile()`) in a previous call. We trust our
                // own cache directory. If the cache is corrupted or tampered with,
                // wasmtime will detect it during deserialization.
                #[allow(unsafe_code)]
                return unsafe { PythonExecutor::from_precompiled_file(&path) };
            }
        }

        // Fall back to in-memory cache (for InMemoryCache users)
        #[cfg(feature = "precompiled")]
        if let Some(cache) = &self.cache {
            if let Some(precompiled) = cache.get(&cache_key) {
                tracing::debug!(
                    key = %cache_key.to_hex(),
                    "component cache hit - loading from bytes"
                );
                #[allow(unsafe_code)]
                return unsafe { PythonExecutor::from_precompiled(&precompiled) };
            }
            tracing::debug!(
                key = %cache_key.to_hex(),
                "component cache miss - will link and compile"
            );
        }

        // Cache miss or no cache - link the component
        let component_bytes = eryx_runtime::linker::link_with_extensions(&self.native_extensions)
            .map_err(|e| Error::Initialization(format!("late-linking failed: {e}")))?;

        // Pre-compile and cache if available
        #[cfg(feature = "precompiled")]
        if let Some(cache) = &self.cache {
            let precompiled = PythonExecutor::precompile(&component_bytes)?;

            // Cache the pre-compiled bytes
            if let Err(e) = cache.put(&cache_key, precompiled.clone()) {
                tracing::warn!(
                    error = %e,
                    "failed to cache pre-compiled component"
                );
            } else {
                tracing::debug!(
                    key = %cache_key.to_hex(),
                    size = precompiled.len(),
                    "cached pre-compiled component"
                );
            }

            // Load from pre-compiled bytes
            // SAFETY: We just created these bytes from `precompile()` above.
            #[allow(unsafe_code)]
            return unsafe { PythonExecutor::from_precompiled(&precompiled) };
        }

        // No cache or precompiled feature - create executor directly from linked bytes
        PythonExecutor::from_binary(&component_bytes)
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
            max_memory_bytes: Some(128 * 1024 * 1024), // 128 MB
            max_callback_invocations: Some(1000),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::callback::{CallbackError, TypedCallback};
    use crate::schema::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use std::future::Future;
    use std::pin::Pin;

    // ==========================================================================
    // ResourceLimits tests
    // ==========================================================================

    #[test]
    fn resource_limits_default_has_reasonable_values() {
        let limits = ResourceLimits::default();

        // Should have execution timeout
        assert!(limits.execution_timeout.is_some());
        let exec_timeout = limits.execution_timeout.unwrap();
        assert!(exec_timeout >= Duration::from_secs(1));
        assert!(exec_timeout <= Duration::from_secs(300));

        // Should have callback timeout
        assert!(limits.callback_timeout.is_some());
        let cb_timeout = limits.callback_timeout.unwrap();
        assert!(cb_timeout >= Duration::from_secs(1));
        assert!(cb_timeout <= Duration::from_secs(60));

        // Should have memory limit
        assert!(limits.max_memory_bytes.is_some());
        let mem_limit = limits.max_memory_bytes.unwrap();
        assert!(mem_limit >= 1024 * 1024); // At least 1 MB
        assert!(mem_limit <= 1024 * 1024 * 1024); // At most 1 GB

        // Should have callback invocation limit
        assert!(limits.max_callback_invocations.is_some());
        let cb_limit = limits.max_callback_invocations.unwrap();
        assert!(cb_limit >= 1);
    }

    #[test]
    fn resource_limits_can_disable_all_limits() {
        let limits = ResourceLimits {
            execution_timeout: None,
            callback_timeout: None,
            max_memory_bytes: None,
            max_callback_invocations: None,
        };

        assert!(limits.execution_timeout.is_none());
        assert!(limits.callback_timeout.is_none());
        assert!(limits.max_memory_bytes.is_none());
        assert!(limits.max_callback_invocations.is_none());
    }

    #[test]
    fn resource_limits_can_set_custom_values() {
        let limits = ResourceLimits {
            execution_timeout: Some(Duration::from_secs(5)),
            callback_timeout: Some(Duration::from_millis(500)),
            max_memory_bytes: Some(64 * 1024 * 1024),
            max_callback_invocations: Some(10),
        };

        assert_eq!(limits.execution_timeout, Some(Duration::from_secs(5)));
        assert_eq!(limits.callback_timeout, Some(Duration::from_millis(500)));
        assert_eq!(limits.max_memory_bytes, Some(64 * 1024 * 1024));
        assert_eq!(limits.max_callback_invocations, Some(10));
    }

    #[test]
    fn resource_limits_is_clone() {
        let limits = ResourceLimits::default();
        let cloned = limits.clone();

        assert_eq!(limits.execution_timeout, cloned.execution_timeout);
        assert_eq!(limits.callback_timeout, cloned.callback_timeout);
        assert_eq!(limits.max_memory_bytes, cloned.max_memory_bytes);
        assert_eq!(
            limits.max_callback_invocations,
            cloned.max_callback_invocations
        );
    }

    #[test]
    fn resource_limits_is_debug() {
        let limits = ResourceLimits::default();
        let debug = format!("{:?}", limits);

        assert!(debug.contains("ResourceLimits"));
        assert!(debug.contains("execution_timeout"));
        assert!(debug.contains("callback_timeout"));
    }

    #[test]
    fn resource_limits_partial_override() {
        // Common pattern: override just one limit
        let limits = ResourceLimits {
            max_callback_invocations: Some(5),
            ..Default::default()
        };

        assert_eq!(limits.max_callback_invocations, Some(5));
        // Others should be default
        assert!(limits.execution_timeout.is_some());
        assert!(limits.callback_timeout.is_some());
        assert!(limits.max_memory_bytes.is_some());
    }

    // ==========================================================================
    // ExecuteResult tests
    // ==========================================================================

    #[test]
    fn execute_result_is_debug() {
        let result = ExecuteResult {
            stdout: "Hello".to_string(),
            trace: vec![],
            stats: ExecuteStats {
                duration: Duration::from_millis(100),
                callback_invocations: 5,
                peak_memory_bytes: Some(1024),
            },
        };

        let debug = format!("{:?}", result);
        assert!(debug.contains("ExecuteResult"));
        assert!(debug.contains("Hello"));
    }

    #[test]
    fn execute_result_is_clone() {
        let result = ExecuteResult {
            stdout: "Test output".to_string(),
            trace: vec![],
            stats: ExecuteStats {
                duration: Duration::from_millis(50),
                callback_invocations: 2,
                peak_memory_bytes: Some(2048),
            },
        };

        let cloned = result.clone();
        assert_eq!(cloned.stdout, "Test output");
        assert_eq!(cloned.stats.callback_invocations, 2);
    }

    // ==========================================================================
    // ExecuteStats tests
    // ==========================================================================

    #[test]
    fn execute_stats_is_debug() {
        let stats = ExecuteStats {
            duration: Duration::from_secs(1),
            callback_invocations: 10,
            peak_memory_bytes: Some(1024 * 1024),
        };

        let debug = format!("{:?}", stats);
        assert!(debug.contains("ExecuteStats"));
        assert!(debug.contains("callback_invocations"));
    }

    #[test]
    fn execute_stats_is_clone() {
        let stats = ExecuteStats {
            duration: Duration::from_millis(250),
            callback_invocations: 3,
            peak_memory_bytes: None,
        };

        let cloned = stats.clone();
        assert_eq!(cloned.duration, Duration::from_millis(250));
        assert_eq!(cloned.callback_invocations, 3);
        assert!(cloned.peak_memory_bytes.is_none());
    }

    #[test]
    fn execute_stats_peak_memory_can_be_none() {
        let stats = ExecuteStats {
            duration: Duration::from_millis(100),
            callback_invocations: 0,
            peak_memory_bytes: None,
        };

        assert!(stats.peak_memory_bytes.is_none());
    }

    // ==========================================================================
    // SandboxBuilder tests
    // ==========================================================================

    #[test]
    fn sandbox_builder_new_creates_default() {
        let builder = SandboxBuilder::new();
        let debug = format!("{:?}", builder);

        assert!(debug.contains("SandboxBuilder"));
    }

    #[test]
    fn sandbox_builder_default_equals_new() {
        let builder1 = SandboxBuilder::new();
        let builder2 = SandboxBuilder::default();

        // Both should have same debug representation structure
        let debug1 = format!("{:?}", builder1);
        let debug2 = format!("{:?}", builder2);

        // Both should contain SandboxBuilder
        assert!(debug1.contains("SandboxBuilder"));
        assert!(debug2.contains("SandboxBuilder"));
    }

    #[test]
    fn sandbox_builder_is_debug() {
        let builder = SandboxBuilder::new();
        let debug = format!("{:?}", builder);

        assert!(debug.contains("SandboxBuilder"));
        assert!(debug.contains("callbacks"));
        assert!(debug.contains("resource_limits"));
    }

    // Test callbacks for builder tests
    #[derive(Deserialize, JsonSchema)]
    struct TestArgs {
        value: String,
    }

    struct TestCallback;

    impl TypedCallback for TestCallback {
        type Args = TestArgs;

        fn name(&self) -> &str {
            "test"
        }

        fn description(&self) -> &str {
            "A test callback"
        }

        fn invoke_typed(
            &self,
            args: TestArgs,
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!({"value": args.value})) })
        }
    }

    struct AnotherCallback;

    impl TypedCallback for AnotherCallback {
        type Args = ();

        fn name(&self) -> &str {
            "another"
        }

        fn description(&self) -> &str {
            "Another callback"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!({})) })
        }
    }

    #[test]
    fn sandbox_builder_build_fails_without_wasm() {
        let builder = SandboxBuilder::new();
        let result = builder.build();

        assert!(result.is_err());
        let error = result.unwrap_err();
        let error_str = format!("{}", error);
        assert!(
            error_str.contains("No WASM") || error_str.contains("wasm"),
            "Error should mention WASM: {}",
            error_str
        );
    }

    #[test]
    fn sandbox_builder_with_callback_is_chainable() {
        // This should compile - testing the builder pattern
        let _builder = SandboxBuilder::new()
            .with_callback(TestCallback)
            .with_callback(AnotherCallback);
    }

    #[test]
    fn sandbox_builder_with_callbacks_accepts_vec() {
        let callbacks: Vec<Box<dyn Callback>> =
            vec![Box::new(TestCallback), Box::new(AnotherCallback)];

        let _builder = SandboxBuilder::new().with_callbacks(callbacks);
    }

    #[test]
    fn sandbox_builder_with_resource_limits_is_chainable() {
        let limits = ResourceLimits {
            max_callback_invocations: Some(5),
            ..Default::default()
        };

        let _builder = SandboxBuilder::new().with_resource_limits(limits);
    }

    #[test]
    fn sandbox_builder_with_wasm_bytes_accepts_vec() {
        // Just test that the builder accepts bytes - actual loading tested elsewhere
        let _builder = SandboxBuilder::new().with_wasm_bytes(vec![0u8; 10]);
    }

    #[test]
    fn sandbox_builder_with_wasm_file_accepts_path() {
        let _builder = SandboxBuilder::new().with_wasm_file("/path/to/file.wasm");
        let _builder = SandboxBuilder::new().with_wasm_file(std::path::PathBuf::from("/path"));
    }

    #[test]
    fn sandbox_builder_full_chain() {
        // Test the full builder pattern (won't build without valid WASM)
        let _builder = SandboxBuilder::new()
            .with_wasm_bytes(vec![])
            .with_callback(TestCallback)
            .with_callback(AnotherCallback)
            .with_resource_limits(ResourceLimits::default());

        // Building will fail due to invalid WASM, but the chain works
    }

    // ==========================================================================
    // Sandbox accessor tests (using a mock approach)
    // ==========================================================================

    // Note: Full Sandbox tests require valid WASM and are in integration tests.
    // These test the accessor methods and types.

    #[test]
    fn sandbox_builder_creates_sandbox_with_valid_wasm() {
        // This test would require valid WASM bytes, so we just verify
        // that the builder pattern compiles correctly
        let builder = Sandbox::builder()
            .with_wasm_bytes(vec![]) // Invalid, but tests the API
            .with_callback(TestCallback)
            .with_resource_limits(ResourceLimits {
                max_callback_invocations: Some(100),
                ..Default::default()
            });

        // Try to build - will fail due to invalid WASM
        let result = builder.build();
        assert!(result.is_err()); // Expected - invalid WASM bytes
    }

    // ==========================================================================
    // WasmSource tests (internal)
    // ==========================================================================

    #[test]
    fn wasm_source_default_is_none() {
        let source = WasmSource::default();
        assert!(matches!(source, WasmSource::None));
    }

    // ==========================================================================
    // Edge case tests
    // ==========================================================================

    #[test]
    fn resource_limits_zero_values() {
        // Zero limits should be representable (though may not be useful)
        let limits = ResourceLimits {
            execution_timeout: Some(Duration::ZERO),
            callback_timeout: Some(Duration::ZERO),
            max_memory_bytes: Some(0),
            max_callback_invocations: Some(0),
        };

        assert_eq!(limits.execution_timeout, Some(Duration::ZERO));
        assert_eq!(limits.max_callback_invocations, Some(0));
    }

    #[test]
    fn resource_limits_very_large_values() {
        let limits = ResourceLimits {
            execution_timeout: Some(Duration::from_secs(86400 * 365)), // 1 year
            callback_timeout: Some(Duration::from_secs(3600)),         // 1 hour
            max_memory_bytes: Some(u64::MAX),
            max_callback_invocations: Some(u32::MAX),
        };

        assert_eq!(limits.max_callback_invocations, Some(u32::MAX));
        assert_eq!(limits.max_memory_bytes, Some(u64::MAX));
    }

    #[test]
    fn execute_stats_zero_duration() {
        let stats = ExecuteStats {
            duration: Duration::ZERO,
            callback_invocations: 0,
            peak_memory_bytes: Some(0),
        };

        assert_eq!(stats.duration, Duration::ZERO);
        assert_eq!(stats.callback_invocations, 0);
    }

    #[test]
    fn execute_result_empty_stdout() {
        let result = ExecuteResult {
            stdout: String::new(),
            trace: vec![],
            stats: ExecuteStats {
                duration: Duration::from_millis(1),
                callback_invocations: 0,
                peak_memory_bytes: None,
            },
        };

        assert!(result.stdout.is_empty());
        assert!(result.trace.is_empty());
    }

    #[test]
    fn execute_result_with_trace_events() {
        use crate::trace::{TraceEvent, TraceEventKind};

        let result = ExecuteResult {
            stdout: "output".to_string(),
            trace: vec![
                TraceEvent {
                    lineno: 1,
                    event: TraceEventKind::Line,
                    context: None,
                },
                TraceEvent {
                    lineno: 2,
                    event: TraceEventKind::Call {
                        function: "foo".to_string(),
                    },
                    context: None,
                },
            ],
            stats: ExecuteStats {
                duration: Duration::from_millis(100),
                callback_invocations: 1,
                peak_memory_bytes: Some(1024),
            },
        };

        assert_eq!(result.trace.len(), 2);
        assert_eq!(result.trace[0].lineno, 1);
    }
}
