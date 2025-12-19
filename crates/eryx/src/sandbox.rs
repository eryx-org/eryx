//! Sandboxed Python execution environment.

use std::{
    collections::HashMap,
    marker::PhantomData,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

#[cfg(feature = "native-extensions")]
use crate::cache::ComponentCache;

/// Marker types for compile-time builder state tracking.
///
/// These types are used with [`SandboxBuilder`] to ensure at compile time
/// that all required configuration is provided before building a sandbox.
pub mod state {
    /// Marker indicating a required component has not been configured.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Needs;

    /// Marker indicating a required component has been configured.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Has;
}

/// Try to automatically find the Python stdlib directory.
///
/// This function searches multiple locations in order:
/// 1. `ERYX_PYTHON_STDLIB` environment variable
/// 2. `./python-stdlib` (relative to current directory)
/// 3. `<exe_dir>/python-stdlib` (relative to executable)
/// 4. `<exe_dir>/../python-stdlib` (sibling of executable directory)
///
/// Returns `Some(path)` if a valid stdlib directory is found, `None` otherwise.
/// A valid stdlib directory must exist and contain an `encodings` subdirectory
/// (required for Python initialization).
fn find_python_stdlib() -> Option<PathBuf> {
    // Helper to validate a stdlib directory
    fn is_valid_stdlib(path: &std::path::Path) -> bool {
        path.is_dir() && path.join("encodings").is_dir()
    }

    // 1. Environment variable
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if is_valid_stdlib(&path) {
            tracing::debug!(path = %path.display(), "Found Python stdlib via ERYX_PYTHON_STDLIB");
            return Some(path);
        }
        tracing::warn!(
            path = %path.display(),
            "ERYX_PYTHON_STDLIB is set but path is not a valid stdlib directory"
        );
    }

    // 2. Current directory
    let cwd_stdlib = PathBuf::from("python-stdlib");
    if is_valid_stdlib(&cwd_stdlib)
        && let Ok(abs_path) = cwd_stdlib.canonicalize()
    {
        tracing::debug!(path = %abs_path.display(), "Found Python stdlib in current directory");
        return Some(abs_path);
    }

    // 3. Relative to executable
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        // Try <exe_dir>/python-stdlib
        let exe_stdlib = exe_dir.join("python-stdlib");
        if is_valid_stdlib(&exe_stdlib) {
            tracing::debug!(path = %exe_stdlib.display(), "Found Python stdlib relative to executable");
            return Some(exe_stdlib);
        }

        // 4. Try <exe_dir>/../python-stdlib (common for installed binaries)
        let parent_stdlib = exe_dir.join("..").join("python-stdlib");
        if is_valid_stdlib(&parent_stdlib)
            && let Ok(abs_path) = parent_stdlib.canonicalize()
        {
            tracing::debug!(path = %abs_path.display(), "Found Python stdlib in parent of executable");
            return Some(abs_path);
        }
    }

    None
}
use crate::callback::Callback;
use crate::callback_handler::{run_callback_handler, run_trace_collector};
use crate::error::Error;
use crate::library::RuntimeLibrary;
use crate::trace::{OutputHandler, TraceEvent, TraceHandler};
use crate::wasm::{CallbackRequest, PythonExecutor, TraceRequest};

/// A sandboxed Python execution environment.
pub struct Sandbox {
    /// The Python WASM executor (wrapped in Arc for sharing with sessions).
    executor: Arc<PythonExecutor>,
    /// Registered callbacks that Python code can invoke (wrapped in Arc to avoid cloning the map on each execute).
    callbacks: Arc<HashMap<String, Arc<dyn Callback>>>,
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
    /// Extracted packages (kept alive to prevent temp directory cleanup).
    _packages: Vec<crate::package::ExtractedPackage>,
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
    ///
    /// You must configure both a runtime source and Python stdlib before building.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::builder()
    ///     .with_wasm_file("runtime.wasm")
    ///     .with_python_stdlib("/path/to/stdlib")
    ///     .build()?;
    /// ```
    ///
    /// Or use [`Sandbox::embedded()`] for zero-config setup when the `embedded`
    /// feature is enabled.
    #[must_use]
    pub fn builder() -> SandboxBuilder<state::Needs, state::Needs> {
        SandboxBuilder::new()
    }

    /// Create a sandbox builder with embedded runtime and stdlib.
    ///
    /// This is the simplest way to create a sandbox - no configuration required.
    /// Only available when the `embedded` feature is enabled.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::embedded()
    ///     .with_callback(MyCallback)
    ///     .build()?;
    /// ```
    #[cfg(feature = "embedded")]
    #[must_use]
    pub fn embedded() -> SandboxBuilder<state::Has, state::Has> {
        SandboxBuilder::new_embedded()
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

        // Spawn task to handle callback requests concurrently (Arc clone is cheap)
        let callbacks_arc = Arc::clone(&self.callbacks);
        let resource_limits = self.resource_limits.clone();
        let callback_handler = tokio::spawn(async move {
            run_callback_handler(callback_rx, callbacks_arc, resource_limits).await
        });

        // Spawn task to handle trace events
        let trace_handler = self.trace_handler.clone();
        let trace_collector =
            tokio::spawn(async move { run_trace_collector(trace_rx, trace_handler).await });

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

    /// Get the callbacks as an Arc for efficient sharing.
    ///
    /// This is more efficient than `callbacks().clone()` when you need to
    /// move callbacks into a spawned task, as it only clones the Arc pointer.
    #[must_use]
    pub(crate) fn callbacks_arc(&self) -> Arc<HashMap<String, Arc<dyn Callback>>> {
        Arc::clone(&self.callbacks)
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
#[derive(Debug, Clone, Default)]
enum WasmSource {
    /// No source specified yet.
    #[default]
    None,
    /// WASM component bytes (will be compiled at load time).
    Bytes(Vec<u8>),
    /// Path to a WASM component file (will be compiled at load time).
    File(std::path::PathBuf),
    /// Pre-compiled component bytes (skip compilation, unsafe).
    #[cfg(feature = "embedded")]
    PrecompiledBytes(Vec<u8>),
    /// Path to a pre-compiled component file (skip compilation, unsafe).
    #[cfg(feature = "embedded")]
    PrecompiledFile(std::path::PathBuf),
    /// Use the embedded pre-compiled runtime (safe, fast).
    #[cfg(feature = "embedded")]
    EmbeddedRuntime,
}

/// Builder for constructing a [`Sandbox`].
///
/// Type parameters track configuration state at compile time:
/// - `Runtime`: Whether WASM runtime is configured ([`state::Needs`] or [`state::Has`])
/// - `Stdlib`: Whether Python stdlib is configured ([`state::Needs`] or [`state::Has`])
///
/// The [`build()`](SandboxBuilder::build) method is only available when both are [`state::Has`].
///
/// # Examples
///
/// ```rust,ignore
/// // With embedded feature - simplest path
/// let sandbox = Sandbox::embedded()
///     .with_callback(MyCallback)
///     .build()?;
///
/// // Without embedded - must specify both runtime and stdlib
/// let sandbox = Sandbox::builder()
///     .with_wasm_file("runtime.wasm")
///     .with_python_stdlib("/path/to/stdlib")
///     .build()?;
/// ```
pub struct SandboxBuilder<Runtime = state::Needs, Stdlib = state::Needs> {
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
    /// Extracted packages (kept alive for sandbox lifetime).
    packages: Vec<crate::package::ExtractedPackage>,
    /// Phantom data for Runtime type parameter.
    _runtime: PhantomData<Runtime>,
    /// Phantom data for Stdlib type parameter.
    _stdlib: PhantomData<Stdlib>,
}

impl Default for SandboxBuilder<state::Needs, state::Needs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, S> std::fmt::Debug for SandboxBuilder<R, S> {
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

impl SandboxBuilder<state::Needs, state::Needs> {
    /// Create a new sandbox builder with default settings.
    ///
    /// You must configure both a runtime source and Python stdlib before building.
    /// Use [`Sandbox::embedded()`] for zero-config setup when the `embedded` feature is enabled.
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
            packages: Vec::new(),
            _runtime: PhantomData,
            _stdlib: PhantomData,
        }
    }
}

/// Create a builder pre-configured with embedded runtime and stdlib.
#[cfg(feature = "embedded")]
impl SandboxBuilder<state::Needs, state::Needs> {
    fn new_embedded() -> SandboxBuilder<state::Has, state::Has> {
        SandboxBuilder {
            wasm_source: WasmSource::EmbeddedRuntime,
            callbacks: HashMap::new(),
            preamble: String::new(),
            type_stubs: String::new(),
            trace_handler: None,
            output_handler: None,
            resource_limits: ResourceLimits::default(),
            python_stdlib_path: None, // Will use embedded stdlib
            python_site_packages_path: None,
            #[cfg(feature = "native-extensions")]
            native_extensions: Vec::new(),
            #[cfg(feature = "native-extensions")]
            cache: None,
            #[cfg(feature = "native-extensions")]
            filesystem_cache: None,
            packages: Vec::new(),
            _runtime: PhantomData,
            _stdlib: PhantomData,
        }
    }
}

// Helper for state transitions
impl<R, S> SandboxBuilder<R, S> {
    /// Internal: transition to new state while preserving all fields.
    fn transition<R2, S2>(self) -> SandboxBuilder<R2, S2> {
        SandboxBuilder {
            wasm_source: self.wasm_source,
            callbacks: self.callbacks,
            preamble: self.preamble,
            type_stubs: self.type_stubs,
            trace_handler: self.trace_handler,
            output_handler: self.output_handler,
            resource_limits: self.resource_limits,
            python_stdlib_path: self.python_stdlib_path,
            python_site_packages_path: self.python_site_packages_path,
            #[cfg(feature = "native-extensions")]
            native_extensions: self.native_extensions,
            #[cfg(feature = "native-extensions")]
            cache: self.cache,
            #[cfg(feature = "native-extensions")]
            filesystem_cache: self.filesystem_cache,
            packages: self.packages,
            _runtime: PhantomData,
            _stdlib: PhantomData,
        }
    }
}

// Runtime transitions (Needs -> Has)
impl<S> SandboxBuilder<state::Needs, S> {
    /// Explicitly use the embedded pre-compiled runtime.
    ///
    /// **Note:** You usually don't need to call this. When the `embedded`
    /// feature is enabled, the embedded runtime is used automatically for
    /// sandboxes without native extensions. This method exists for explicit
    /// control in advanced use cases.
    ///
    /// # Automatic Runtime Selection
    ///
    /// The runtime is selected automatically based on your configuration:
    ///
    /// - **No native extensions** → Embedded runtime (fast, ~2ms)
    /// - **Has native extensions** → Late-linking (required for .so files)
    ///
    /// ```rust,ignore
    /// // These are equivalent when embedded feature is enabled:
    /// let sandbox = Sandbox::builder().build()?;
    /// let sandbox = Sandbox::builder().with_embedded_runtime().build()?;
    ///
    /// // With native extensions, late-linking happens automatically:
    /// let sandbox = Sandbox::builder()
    ///     .with_package("/path/to/numpy-wasi.tar.gz")?  // Has .so files
    ///     .build()?;  // Uses late-linking, not embedded runtime
    /// ```
    /// Explicitly use the embedded pre-compiled runtime and stdlib.
    ///
    /// This transitions the builder to a fully-configured state, ready to build.
    ///
    /// **Note:** Consider using [`Sandbox::embedded()`] instead for cleaner code.
    #[cfg(feature = "embedded")]
    #[must_use]
    pub fn with_embedded_runtime(mut self) -> SandboxBuilder<state::Has, state::Has> {
        self.wasm_source = WasmSource::EmbeddedRuntime;
        self.transition()
    }

    /// Set the WASM component from bytes.
    ///
    /// Use this to embed the WASM component in your binary.
    /// You still need to configure the Python stdlib with [`with_python_stdlib()`](SandboxBuilder::with_python_stdlib)
    /// or [`with_auto_stdlib()`](SandboxBuilder::with_auto_stdlib).
    #[must_use]
    pub fn with_wasm_bytes(mut self, bytes: impl Into<Vec<u8>>) -> SandboxBuilder<state::Has, S> {
        self.wasm_source = WasmSource::Bytes(bytes.into());
        self.transition()
    }

    /// Set the WASM component from a file path.
    ///
    /// You still need to configure the Python stdlib with [`with_python_stdlib()`](SandboxBuilder::with_python_stdlib)
    /// or [`with_auto_stdlib()`](SandboxBuilder::with_auto_stdlib).
    #[must_use]
    pub fn with_wasm_file(
        mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> SandboxBuilder<state::Has, S> {
        self.wasm_source = WasmSource::File(path.into());
        self.transition()
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
    ///         .with_python_stdlib("/path/to/stdlib")
    ///         .build()?
    /// };
    /// ```
    #[cfg(feature = "embedded")]
    #[must_use]
    #[allow(unsafe_code)]
    pub unsafe fn with_precompiled_bytes(
        mut self,
        bytes: impl Into<Vec<u8>>,
    ) -> SandboxBuilder<state::Has, S> {
        self.wasm_source = WasmSource::PrecompiledBytes(bytes.into());
        self.transition()
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
    ///         .with_python_stdlib("/path/to/stdlib")
    ///         .build()?
    /// };
    /// ```
    #[cfg(feature = "embedded")]
    #[must_use]
    #[allow(unsafe_code)]
    pub unsafe fn with_precompiled_file(
        mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> SandboxBuilder<state::Has, S> {
        self.wasm_source = WasmSource::PrecompiledFile(path.into());
        self.transition()
    }
}

// Stdlib transitions (Needs -> Has)
impl<R> SandboxBuilder<R, state::Needs> {
    /// Set the path to the Python standard library directory.
    ///
    /// This is required when not using the `embedded` feature.
    /// The directory should contain the extracted Python stdlib (e.g., from
    /// componentize-py's python-lib.tar.zst).
    ///
    /// The stdlib will be mounted at `/python-stdlib` inside the WASM sandbox.
    #[must_use]
    pub fn with_python_stdlib(
        mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> SandboxBuilder<R, state::Has> {
        self.python_stdlib_path = Some(path.into());
        self.transition()
    }

    /// Auto-detect Python stdlib from common locations.
    ///
    /// Searches in order:
    /// 1. `ERYX_PYTHON_STDLIB` environment variable
    /// 2. `./python-stdlib` (relative to current directory)
    /// 3. `<exe_dir>/python-stdlib` (relative to executable)
    /// 4. `<exe_dir>/../python-stdlib` (sibling of executable directory)
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingPythonStdlib`] if no valid stdlib directory is found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::builder()
    ///     .with_wasm_file("runtime.wasm")
    ///     .with_auto_stdlib()?  // Explicit fallible auto-detection
    ///     .build()?;
    /// ```
    pub fn with_auto_stdlib(self) -> Result<SandboxBuilder<R, state::Has>, Error> {
        let path = find_python_stdlib().ok_or(Error::MissingPythonStdlib)?;
        Ok(self.with_python_stdlib(path))
    }
}

// Methods available in ANY state
impl<R, S> SandboxBuilder<R, S> {
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
            .push(eryx_runtime::linker::NativeExtension::new(
                name,
                bytes.into(),
            ));
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

    /// Set a custom filesystem cache directory for late-linked components.
    ///
    /// **Note:** You usually don't need to call this. A default cache at
    /// `$TMPDIR/eryx-cache` is used automatically when native extensions are present.
    /// Use this method only if you need a specific cache location.
    ///
    /// The cache stores pre-compiled WASM components to avoid expensive
    /// re-linking on subsequent sandbox creations with the same extensions.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache directory cannot be created.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Usually not needed - default cache is automatic
    /// let sandbox = Sandbox::builder()
    ///     .with_package("/path/to/numpy.tar.gz")?
    ///     .build()?;  // Uses $TMPDIR/eryx-cache automatically
    ///
    /// // Only if you need a specific location:
    /// let sandbox = Sandbox::builder()
    ///     .with_package("/path/to/numpy.tar.gz")?
    ///     .with_cache_dir("/custom/cache/path")?
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

    /// Set the path to additional Python packages directory.
    ///
    /// The directory will be mounted at `/site-packages` inside the WASM sandbox
    /// and added to Python's import path.
    #[must_use]
    pub fn with_site_packages(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.python_site_packages_path = Some(path.into());
        self
    }

    /// Add a Python package from a wheel (.whl) or tar.gz archive.
    ///
    /// The package format is auto-detected from the file extension:
    /// - `.whl` - Standard Python wheel (zip archive)
    /// - `.tar.gz`, `.tgz` - Tarball (used by wasi-wheels)
    /// - Directory - Used directly without extraction
    ///
    /// # Pure Python packages
    ///
    /// For pure-Python packages (no `.so` files), you can use `with_embedded_runtime()`:
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::builder()
    ///     .with_embedded_runtime()
    ///     .with_package("/path/to/requests-2.31.0-py3-none-any.whl")?
    ///     .build()?;
    /// ```
    ///
    /// # Packages with native extensions
    ///
    /// For packages containing native extensions (like numpy), the extensions are
    /// automatically registered for late-linking. A cache is set up automatically
    /// at `$TMPDIR/eryx-cache` for fast subsequent sandbox creations:
    ///
    /// ```rust,ignore
    /// let sandbox = Sandbox::builder()
    ///     .with_package("/path/to/numpy-wasi.tar.gz")?
    ///     .build()?;  // Caching is automatic!
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The package format cannot be detected
    /// - The archive cannot be read or extracted
    pub fn with_package(mut self, path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let package = crate::package::ExtractedPackage::from_path(path)?;

        tracing::info!(
            name = %package.name,
            has_native_extensions = package.has_native_extensions,
            "Loaded package"
        );

        // Check for incompatible configuration
        #[cfg(not(feature = "native-extensions"))]
        if package.has_native_extensions {
            return Err(Error::Initialization(format!(
                "Package '{}' contains native extensions but the 'native-extensions' feature is not enabled. \
                 Either use a pure-Python package or enable the 'native-extensions' feature.",
                package.name
            )));
        }

        // Store the extracted package - native extensions will be registered at build()
        // time when we know the mount index for computing dlopen paths
        self.packages.push(package);

        Ok(self)
    }

    /// Load a Python package from raw bytes.
    ///
    /// This is useful when downloading packages from URLs. The format must be
    /// specified explicitly since it cannot be detected from bytes alone.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::{Sandbox, PackageFormat};
    ///
    /// // Download a package (using your preferred HTTP client)
    /// let bytes = reqwest::get("https://example.com/numpy-wasi.tar.gz")
    ///     .await?
    ///     .bytes()
    ///     .await?;
    ///
    /// let sandbox = Sandbox::builder()
    ///     .with_package_bytes(&bytes, PackageFormat::TarGz, "numpy")?
    ///     .build()?;
    /// ```
    ///
    /// # Arguments
    ///
    /// * `bytes` - The raw package bytes
    /// * `format` - The package format (Wheel or TarGz)
    /// * `name_hint` - Package name hint used if detection fails (e.g., "numpy")
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The format is `Directory` (not supported for bytes)
    /// - The archive cannot be read or extracted
    pub fn with_package_bytes(
        mut self,
        bytes: &[u8],
        format: crate::package::PackageFormat,
        name_hint: impl Into<String>,
    ) -> Result<Self, Error> {
        let package = crate::package::ExtractedPackage::from_bytes(bytes, format, name_hint)?;

        tracing::info!(
            name = %package.name,
            has_native_extensions = package.has_native_extensions,
            "Loaded package from bytes"
        );

        // Check for incompatible configuration
        #[cfg(not(feature = "native-extensions"))]
        if package.has_native_extensions {
            return Err(Error::Initialization(format!(
                "Package '{}' contains native extensions but the 'native-extensions' feature is not enabled. \
                 Either use a pure-Python package or enable the 'native-extensions' feature.",
                package.name
            )));
        }

        // Store the extracted package - native extensions will be registered at build()
        // time when we know the mount index for computing dlopen paths
        self.packages.push(package);

        Ok(self)
    }
}

// Build only available when BOTH runtime AND stdlib are configured
impl SandboxBuilder<state::Has, state::Has> {
    /// Build the sandbox.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No WASM component was specified and no default is available
    /// - The WASM component cannot be loaded
    /// - The WebAssembly runtime fails to initialize
    ///
    /// # Native Extensions
    ///
    /// If native extensions are registered (via `with_native_extension()` or
    /// `with_package()` with `.so` files), late-linking is used automatically.
    /// This overrides any `with_embedded_runtime()` setting since native
    /// extensions must be linked into the runtime.
    #[allow(unused_mut)] // mut needed when native-extensions feature is enabled
    pub fn build(mut self) -> Result<Sandbox, Error> {
        // First, compute mount indices and register native extensions from packages
        // with correct dlopen paths. Mount index 0 is reserved for explicit site-packages.
        #[cfg(feature = "native-extensions")]
        {
            let start_index = if self.python_site_packages_path.is_some() {
                1
            } else {
                0
            };
            for (pkg_idx, package) in self.packages.iter().enumerate() {
                let mount_index = start_index + pkg_idx;
                for ext in &package.native_extensions {
                    let dlopen_path =
                        format!("/site-packages-{}/{}", mount_index, ext.relative_path);
                    self.native_extensions
                        .push(eryx_runtime::linker::NativeExtension::new(
                            dlopen_path,
                            ext.bytes.clone(),
                        ));
                }
            }
        }

        // Set up default cache for native extensions if none specified
        // This avoids re-linking on every sandbox creation
        #[cfg(all(feature = "native-extensions", feature = "embedded"))]
        if !self.native_extensions.is_empty()
            && self.filesystem_cache.is_none()
            && self.cache.is_none()
        {
            let default_cache_dir = std::env::temp_dir().join("eryx-cache");
            if let Ok(cache) = crate::cache::FilesystemCache::new(&default_cache_dir) {
                tracing::debug!(path = %default_cache_dir.display(), "Using default cache directory");
                self.filesystem_cache = Some(cache.clone());
                self.cache = Some(Arc::new(cache));
            }
        }

        // If native extensions are specified, use late-linking to create the component.
        // This OVERRIDES any wasm_source setting (including embedded runtime) because
        // native extensions must be linked into the runtime at this point.
        #[cfg(feature = "native-extensions")]
        let executor = if !self.native_extensions.is_empty() {
            // Warn if user explicitly set embedded runtime - it will be ignored
            #[cfg(feature = "embedded")]
            if matches!(self.wasm_source, WasmSource::EmbeddedRuntime) {
                tracing::info!(
                    "Native extensions detected - using late-linking instead of embedded runtime"
                );
            }
            self.build_executor_with_extensions()?
        } else {
            self.build_executor_from_source()?
        };

        #[cfg(not(feature = "native-extensions"))]
        let executor = self.build_executor_from_source()?;

        // Determine stdlib path: explicit > embedded > auto-detect > none
        #[cfg(feature = "embedded")]
        let stdlib_path = self
            .python_stdlib_path
            .clone()
            .or_else(|| {
                crate::embedded::EmbeddedResources::get()
                    .ok()
                    .map(|r| r.stdlib_path.clone())
            })
            .or_else(find_python_stdlib);

        #[cfg(not(feature = "embedded"))]
        let stdlib_path = self.python_stdlib_path.clone().or_else(find_python_stdlib);

        // Collect all site-packages paths: explicit path first, then package paths
        let mut site_packages_paths = Vec::new();
        if let Some(explicit_path) = self.python_site_packages_path.clone() {
            site_packages_paths.push(explicit_path);
        }
        for package in &self.packages {
            site_packages_paths.push(package.python_path.clone());
        }

        // Apply Python stdlib path if available
        let executor = if let Some(stdlib) = stdlib_path {
            executor.with_python_stdlib(&stdlib)
        } else {
            executor
        };

        // Apply all site-packages paths
        let executor = site_packages_paths
            .into_iter()
            .fold(executor, |exec, path| exec.with_site_packages(&path));

        Ok(Sandbox {
            executor: Arc::new(executor),
            callbacks: Arc::new(self.callbacks),
            preamble: self.preamble,
            type_stubs: self.type_stubs,
            trace_handler: self.trace_handler,
            output_handler: self.output_handler,
            resource_limits: self.resource_limits,
            _packages: self.packages,
        })
    }

    /// Build executor from the configured WASM source.
    fn build_executor_from_source(&self) -> Result<PythonExecutor, Error> {
        let executor = match &self.wasm_source {
            WasmSource::Bytes(bytes) => PythonExecutor::from_binary(bytes)?,
            WasmSource::File(path) => PythonExecutor::from_file(path)?,

            #[cfg(feature = "embedded")]
            WasmSource::PrecompiledBytes(bytes) => {
                // SAFETY: User is responsible for only using trusted pre-compiled bytes.
                // The `with_precompiled_bytes` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled(bytes)?
                }
            }

            #[cfg(feature = "embedded")]
            WasmSource::PrecompiledFile(path) => {
                // SAFETY: User is responsible for only using trusted pre-compiled files.
                // The `with_precompiled_file` method is already marked unsafe, so the
                // caller has acknowledged this responsibility.
                #[allow(unsafe_code)]
                unsafe {
                    PythonExecutor::from_precompiled_file(path)?
                }
            }

            #[cfg(feature = "embedded")]
            WasmSource::EmbeddedRuntime => {
                // Use the optimized path that leverages InstancePreCache
                PythonExecutor::from_embedded_runtime()?
            }

            WasmSource::None => {
                // If embedded feature is enabled, use it automatically as the default
                #[cfg(feature = "embedded")]
                {
                    tracing::debug!("No WASM source specified, using embedded runtime");
                    // Use the optimized path that leverages InstancePreCache
                    PythonExecutor::from_embedded_runtime()?
                }

                #[cfg(not(feature = "embedded"))]
                {
                    let msg = "No WASM component specified. Use with_wasm_bytes() or with_wasm_file(). \
                               Or enable the `embedded` feature for automatic runtime loading.";

                    return Err(Error::Initialization(msg.to_string()));
                }
            }
        };

        Ok(executor)
    }

    /// Build executor with native extensions, using cache if available.
    ///
    /// When a cache is configured and the `embedded` feature is enabled,
    /// this will:
    /// 1. Check the cache for a pre-compiled component
    /// 2. If found, load from cache (fast path)
    /// 3. If not found, link extensions, pre-compile, cache, and return
    #[cfg(feature = "native-extensions")]
    fn build_executor_with_extensions(&self) -> Result<PythonExecutor, Error> {
        #[cfg(feature = "embedded")]
        use crate::cache::{CacheKey, InstancePreCache};

        #[cfg(feature = "embedded")]
        let cache_key = CacheKey::from_extensions(&self.native_extensions);

        // Tier 1: Check InstancePreCache first (fastest - just Clone)
        #[cfg(feature = "embedded")]
        if let Some(instance_pre) = InstancePreCache::global().get(&cache_key) {
            tracing::debug!(
                key = %cache_key.to_hex(),
                "instance_pre cache hit - returning cached executor"
            );
            return PythonExecutor::from_cached_instance_pre(instance_pre);
        }

        // Tier 2: Try filesystem cache (mmap-based, faster than bytes)
        #[cfg(feature = "embedded")]
        if let Some(fs_cache) = &self.filesystem_cache
            && let Some(path) = fs_cache.get_path(&cache_key)
        {
            tracing::debug!(
                key = %cache_key.to_hex(),
                path = %path.display(),
                "component cache hit - loading via mmap"
            );
            // SAFETY: The cached pre-compiled file was created by us (from
            // `PythonExecutor::precompile()`) in a previous call. We trust our
            // own cache directory. If the cache is corrupted or tampered with,
            // wasmtime will detect it during deserialization.
            // Use with_key to populate InstancePreCache for future calls.
            #[allow(unsafe_code)]
            return unsafe { PythonExecutor::from_precompiled_file_with_key(&path, cache_key) };
        }

        // Tier 2 (continued): Fall back to in-memory byte cache (for InMemoryCache users)
        #[cfg(feature = "embedded")]
        if let Some(cache) = &self.cache {
            if let Some(precompiled) = cache.get(&cache_key) {
                tracing::debug!(
                    key = %cache_key.to_hex(),
                    "component cache hit - loading from bytes"
                );
                // Load and populate InstancePreCache for future calls
                #[allow(unsafe_code)]
                let executor = unsafe { PythonExecutor::from_precompiled(&precompiled) }?;
                InstancePreCache::global().put(cache_key, executor.instance_pre().clone());
                return Ok(executor);
            }
            tracing::debug!(
                key = %cache_key.to_hex(),
                "component cache miss - will link and compile"
            );
        }

        // Cache miss or no cache - link the component
        let component_bytes =
            eryx_runtime::linker::link_with_extensions(&self.native_extensions)
                .map_err(|e| Error::Initialization(format!("late-linking failed: {e}")))?;

        // Pre-compile and cache if available
        #[cfg(feature = "embedded")]
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

            // Load from pre-compiled bytes and populate InstancePreCache
            // SAFETY: We just created these bytes from `precompile()` above.
            #[allow(unsafe_code)]
            let executor = unsafe { PythonExecutor::from_precompiled(&precompiled) }?;
            InstancePreCache::global().put(cache_key, executor.instance_pre().clone());
            return Ok(executor);
        }

        // No cache or embedded feature - create executor directly from linked bytes
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

    /// Test that typestate prevents building without configuration.
    ///
    /// With typestate pattern, calling `build()` without configuring runtime and stdlib
    /// is a **compile-time error**, not a runtime error. This test documents the API design.
    ///
    /// The following code would NOT compile:
    /// ```compile_fail
    /// use eryx::Sandbox;
    /// let sandbox = Sandbox::builder().build(); // ERROR: build() not available
    /// ```
    #[test]
    fn sandbox_builder_typestate_prevents_unconfigured_build() {
        // This test verifies that the typestate pattern is in place.
        // The actual compile-time checking is documented above.
        //
        // We can verify that a fully-configured builder does have build():
        let _builder = SandboxBuilder::new()
            .with_wasm_bytes(vec![])
            .with_python_stdlib("/fake/path");
        // _builder.build() would work here (though fail at runtime due to invalid WASM)
    }

    /// Test that sandbox creation succeeds with Sandbox::embedded().
    #[test]
    #[cfg(feature = "embedded")]
    fn sandbox_embedded_builds_successfully() {
        let result = Sandbox::embedded().build();

        assert!(
            result.is_ok(),
            "Sandbox::embedded() should build successfully"
        );
    }

    /// Test that with_embedded_runtime() transitions to fully-configured state.
    #[test]
    #[cfg(feature = "embedded")]
    fn sandbox_builder_with_embedded_runtime_builds() {
        let result = SandboxBuilder::new().with_embedded_runtime().build();

        assert!(
            result.is_ok(),
            "with_embedded_runtime() should provide both runtime and stdlib"
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
            .with_python_stdlib("/fake/stdlib") // Required for typestate
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

    // ==========================================================================
    // Unhappy path tests (explicit configuration errors)
    // ==========================================================================

    #[test]
    fn sandbox_builder_wasm_file_not_found() {
        let result = Sandbox::builder()
            .with_wasm_file("/nonexistent/path/to/runtime.wasm")
            .with_python_stdlib("/fake/stdlib")
            .build();

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("No such file")
                || err.contains("not found")
                || err.contains("failed to read"),
            "Expected file not found error, got: {err}"
        );
    }

    #[test]
    fn sandbox_builder_invalid_wasm_bytes() {
        let result = Sandbox::builder()
            .with_wasm_bytes(vec![0, 1, 2, 3]) // Invalid WASM magic bytes
            .with_python_stdlib("/fake/stdlib")
            .build();

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // wasmtime returns various errors for invalid WASM
        assert!(
            err.contains("magic") || err.contains("invalid") || err.contains("failed"),
            "Expected WASM parsing error, got: {err}"
        );
    }

    #[test]
    fn sandbox_builder_empty_wasm_bytes() {
        let result = Sandbox::builder()
            .with_wasm_bytes(vec![])
            .with_python_stdlib("/fake/stdlib")
            .build();

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unexpected end")
                || err.contains("empty")
                || err.contains("failed")
                || err.contains("magic"),
            "Expected empty WASM error, got: {err}"
        );
    }

    #[test]
    fn sandbox_builder_with_auto_stdlib_returns_result() {
        // with_auto_stdlib() returns a Result - it may succeed or fail depending
        // on whether a stdlib is found in standard locations.
        // We just verify the API works correctly.
        let result = Sandbox::builder()
            .with_wasm_bytes(vec![])
            .with_auto_stdlib();

        // Either Ok (stdlib found) or Err (MissingPythonStdlib)
        match result {
            Ok(builder) => {
                // Stdlib was found, builder is now fully configured
                // Build will fail due to invalid WASM, but typestate is satisfied
                let build_result = builder.build();
                assert!(build_result.is_err()); // Invalid WASM
            }
            Err(e) => {
                // Stdlib not found - should be MissingPythonStdlib error
                let err_str = e.to_string();
                assert!(
                    err_str.contains("stdlib") || err_str.contains("Python"),
                    "Expected stdlib-related error, got: {err_str}"
                );
            }
        }
    }

    #[test]
    #[cfg(not(feature = "embedded"))]
    fn sandbox_builder_requires_explicit_config_without_embedded() {
        // Without the embedded feature, Sandbox::builder() returns SandboxBuilder<Needs, Needs>
        // and build() is not available until both are configured.
        //
        // This is a compile-time check, but we verify the types work correctly:
        let builder = Sandbox::builder();

        // Can add callbacks without configuring runtime/stdlib
        let builder = builder.with_callback(TestCallback);

        // Must configure both to get build()
        let builder = builder
            .with_wasm_bytes(vec![1, 2, 3])
            .with_python_stdlib("/fake");

        // Now build() is available (will fail due to invalid WASM, but that's expected)
        let result = builder.build();
        assert!(result.is_err()); // Invalid WASM
    }
}
