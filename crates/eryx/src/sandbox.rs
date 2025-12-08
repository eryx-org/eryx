//! Sandboxed Python execution environment.

use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::callback::Callback;
use crate::error::Error;
use crate::library::RuntimeLibrary;
use crate::trace::{OutputHandler, TraceEvent, TraceHandler};

/// A sandboxed Python execution environment.
pub struct Sandbox {
    /// Wasmtime engine for executing WebAssembly.
    #[allow(dead_code)]
    engine: wasmtime::Engine,
    /// Compiled WebAssembly component.
    #[allow(dead_code)]
    component: wasmtime::component::Component,
    /// Registered callbacks that Python code can invoke.
    #[allow(dead_code)]
    callbacks: HashMap<String, Arc<dyn Callback>>,
    /// Python preamble code injected before user code.
    #[allow(dead_code)]
    preamble: String,
    /// Combined type stubs from all libraries.
    type_stubs: String,
    /// Handler for execution trace events.
    #[allow(dead_code)]
    trace_handler: Option<Arc<dyn TraceHandler>>,
    /// Handler for streaming stdout output.
    #[allow(dead_code)]
    output_handler: Option<Arc<dyn OutputHandler>>,
    /// Resource limits for execution.
    #[allow(dead_code)]
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
    #[allow(clippy::unused_async)] // Will be async when WASM execution is implemented
    pub async fn execute(&self, _code: &str) -> Result<ExecuteResult, Error> {
        // TODO: Implement actual WASM execution
        todo!("WASM execution not yet implemented")
    }

    /// Get combined type stubs for all loaded libraries.
    /// Useful for including in LLM context windows.
    #[must_use]
    pub fn type_stubs(&self) -> &str {
        &self.type_stubs
    }
}

/// Builder for constructing a [`Sandbox`].
#[derive(Default)]
pub struct SandboxBuilder {
    callbacks: HashMap<String, Arc<dyn Callback>>,
    preamble: String,
    type_stubs: String,
    trace_handler: Option<Arc<dyn TraceHandler>>,
    output_handler: Option<Arc<dyn OutputHandler>>,
    resource_limits: ResourceLimits,
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
            .finish()
    }
}

impl SandboxBuilder {
    /// Create a new sandbox builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// Returns an error if the WebAssembly runtime fails to initialize.
    pub fn build(self) -> Result<Sandbox, Error> {
        let mut config = wasmtime::Config::new();
        config.async_support(true);
        config.wasm_component_model(true);

        let engine =
            wasmtime::Engine::new(&config).map_err(|e| Error::WasmEngine(e.to_string()))?;

        // TODO: Load actual WASM component
        // For now, create a minimal valid component
        let component = wasmtime::component::Component::new(&engine, "(component)")
            .map_err(Error::WasmComponent)?;

        Ok(Sandbox {
            engine,
            component,
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
