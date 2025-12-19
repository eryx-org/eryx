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

#[cfg(feature = "embedded")]
use crate::cache::{CacheKey, InstancePreCache};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};
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
    /// Peak memory usage in bytes during execution.
    pub peak_memory_bytes: u64,
}

impl ExecutionOutput {
    /// Create a new execution output.
    #[must_use]
    pub fn new(stdout: String, peak_memory_bytes: u64) -> Self {
        Self {
            stdout,
            peak_memory_bytes,
        }
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
// The WIT already declares `invoke` and `execute` as async, wasmtime handles it
// Note: async functions in WIT get prefixed with "[async]" in the component model
wasmtime::component::bindgen!({
    path: "../eryx-runtime/runtime.wit",
    imports: {
        "[async]invoke": async | exact | store | tracing | trappable
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
}

impl std::fmt::Debug for ExecutorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorState")
            .field("wasi", &"<WasiCtx>")
            .field("table", &"<ResourceTable>")
            .field("callback_tx", &self.callback_tx.is_some())
            .field("trace_tx", &self.trace_tx.is_some())
            .field("callbacks", &self.callbacks.len())
            .field(
                "peak_memory_bytes",
                &self.memory_tracker.peak_memory_bytes(),
            )
            .finish()
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

// Host implementation of the WIT-generated sandbox imports trait.
impl SandboxImportsWithStore for HasSelf<ExecutorState> {
    /// Invoke a callback by name with JSON arguments (async).
    fn invoke<T>(
        accessor: &Accessor<T, Self>,
        name: String,
        arguments_json: String,
    ) -> impl ::core::future::Future<Output = wasmtime::Result<Result<String, String>>> + Send {
        tracing::debug!(
            callback = %name,
            args_len = arguments_json.len(),
            "Python invoking callback"
        );

        async move {
            let result =
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
                };
            Ok(result)
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
    /// Each directory will be mounted at `/site-packages-N` inside the WASM sandbox
    /// and added to Python's import path. Can be called multiple times.
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

    /// Create a configured wasmtime engine.
    ///
    /// Uses copy-on-write heap images to defer memory initialization
    /// from instantiation time to first write, improving startup performance.
    fn create_engine() -> std::result::Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.async_support(true);

        // Enable copy-on-write heap images for faster instantiation
        // This defers memory initialization from instantiation time to first write
        config.memory_init_cow(true);

        // Optimize for smaller generated code (slight runtime perf tradeoff)
        // This reduces .cwasm file sizes and memory footprint
        config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);

        // Reduce async stack size from default 2 MiB to 512 KiB
        // Python scripts don't need deep call stacks
        config.async_stack_size(512 * 1024);

        Engine::new(&config).map_err(|e| Error::WasmEngine(e.to_string()))
    }

    /// Create a pre-instantiated component with all imports linked.
    ///
    /// This does the expensive linking work once, so that `execute()` can
    /// quickly instantiate from the template.
    fn create_instance_pre(
        engine: &Engine,
        component: &Component,
    ) -> std::result::Result<SandboxPre<ExecutorState>, Error> {
        let mut linker = Linker::<ExecutorState>::new(engine);

        // Add WASI support (p2 = preview 2)
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| Error::WasmEngine(format!("Failed to add WASI to linker: {e}")))?;

        // Add sandbox bindings
        Sandbox::add_to_linker::<_, HasSelf<ExecutorState>>(&mut linker, |state| state)
            .map_err(|e| Error::WasmEngine(format!("Failed to add sandbox to linker: {e}")))?;

        // Create pre-instantiated component
        // This validates that all imports are satisfied and prepares for fast instantiation
        let instance_pre = linker
            .instantiate_pre(component)
            .map_err(|e| Error::WasmEngine(format!("Failed to create instance_pre: {e}")))?;

        // Wrap in SandboxPre for typed access to exports
        SandboxPre::new(instance_pre)
            .map_err(|e| Error::WasmEngine(format!("Failed to create SandboxPre: {e}")))
    }

    /// Execute Python code with the given callbacks and trace channel.
    ///
    /// This is fast because the expensive linking work was done during
    /// construction. Each call only needs to:
    /// 1. Create a new store with fresh state
    /// 2. Instantiate from the pre-compiled template
    /// 3. Call the execute export
    ///
    /// # Arguments
    ///
    /// * `code` - The Python code to execute
    /// * `callbacks` - Available callbacks that Python code can invoke
    /// * `callback_tx` - Channel for callback requests (None for no callbacks)
    /// * `trace_tx` - Channel for trace events (None for no tracing)
    ///
    /// # Returns
    ///
    /// Returns an [`ExecutionOutput`] on success, or an error message on failure.
    pub async fn execute(
        &self,
        code: &str,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
        memory_limit: Option<u64>,
    ) -> std::result::Result<ExecutionOutput, String> {
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
        let mut pythonpath_parts = Vec::new();
        if self.python_stdlib_path.is_some() {
            pythonpath_parts.push("/python-stdlib".to_string());
        }
        for i in 0..self.python_site_packages_paths.len() {
            pythonpath_parts.push(format!("/site-packages-{i}"));
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
                .map_err(|e| format!("Failed to mount Python stdlib: {e}"))?;
        }

        // Set PYTHONPATH for all configured paths (stdlib and/or site-packages)
        if !pythonpath_parts.is_empty() {
            wasi_builder.env("PYTHONPATH", pythonpath_parts.join(":"));
        }

        // Mount each site-packages directory at a unique path
        for (i, site_packages_path) in self.python_site_packages_paths.iter().enumerate() {
            let mount_path = format!("/site-packages-{i}");
            wasi_builder
                .preopened_dir(
                    site_packages_path,
                    &mount_path,
                    DirPerms::READ,
                    FilePerms::READ,
                )
                .map_err(|e| format!("Failed to mount {mount_path}: {e}"))?;
        }

        let wasi = wasi_builder.build();

        let state = ExecutorState {
            wasi,
            table: ResourceTable::new(),
            callback_tx,
            trace_tx,
            callbacks: callback_infos,
            memory_tracker: MemoryTracker::new(memory_limit),
        };

        // Create store for this execution
        let mut store = Store::new(&self.engine, state);

        // Register the memory tracker as a resource limiter
        store.limiter(|state| &mut state.memory_tracker);

        // Instantiate from the pre-compiled template (fast!)
        let bindings = self
            .instance_pre
            .instantiate_async(&mut store)
            .await
            .map_err(|e| format!("Failed to instantiate component: {e}"))?;

        tracing::debug!(code_len = code.len(), "Executing Python code");

        // Call the async execute export
        let code_owned = code.to_string();

        // run_concurrent returns Result<R, Error> where R is the closure's return type
        let wasmtime_result = store
            .run_concurrent(async |accessor| bindings.call_execute(accessor, code_owned).await)
            .await
            .map_err(|e| format!("WASM execution error: {e:?}"))?;

        // wasmtime_result is wasmtime::Result<Result<String, String>>
        let stdout = wasmtime_result.map_err(|e| format!("WASM execution error: {e:?}"))??;

        // Get peak memory from the store before it's dropped
        let peak_memory_bytes = store.data().memory_tracker.peak_memory_bytes();

        Ok(ExecutionOutput::new(stdout, peak_memory_bytes))
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
}
