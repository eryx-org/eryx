//! WebAssembly runtime setup and WIT bindings.
//!
//! This module handles the wasmtime engine configuration, component loading,
//! and host import implementations for running Python code in the sandbox.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
    wasi: WasiCtx,
    /// Resource table for WASI.
    table: ResourceTable,
    /// Channel to send callback requests to the host.
    callback_tx: Option<mpsc::Sender<CallbackRequest>>,
    /// Channel to send trace events to the host.
    trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    /// Available callbacks for introspection.
    callbacks: Vec<HostCallbackInfo>,
}

impl WasiView for ExecutorState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// Implement the sync imports in SandboxImports
// Host implementation of the sandbox imports
// The generated trait requires implementing invoke (async), list_callbacks, and report_trace
// The bindgen macro generates traits based on the WIT world name.
// For world "sandbox" with package "eryx:sandbox", the Host trait is generated.
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
pub struct PythonExecutor {
    engine: Engine,
    component: Component,
    linker: Linker<ExecutorState>,
}

impl std::fmt::Debug for PythonExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonExecutor")
            .field("engine", &"<wasmtime::Engine>")
            .field("component", &"<wasmtime::Component>")
            .finish_non_exhaustive()
    }
}

impl PythonExecutor {
    /// Create a new executor by loading a WASM component from bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the WASM component cannot be loaded or the
    /// wasmtime engine cannot be configured.
    pub fn from_binary(wasm_bytes: &[u8]) -> std::result::Result<Self, Error> {
        let engine = Self::create_engine()?;
        let component =
            Component::from_binary(&engine, wasm_bytes).map_err(Error::WasmComponent)?;
        let linker = Self::create_linker(&engine)?;

        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// Create a new executor by loading a WASM component from a file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the WASM component
    /// cannot be loaded.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> std::result::Result<Self, Error> {
        let engine = Self::create_engine()?;
        let component =
            Component::from_file(&engine, path.as_ref()).map_err(Error::WasmComponent)?;
        let linker = Self::create_linker(&engine)?;

        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// Create a configured wasmtime engine.
    fn create_engine() -> std::result::Result<Engine, Error> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.async_support(true);

        Engine::new(&config).map_err(|e| Error::WasmEngine(e.to_string()))
    }

    /// Create a linker with WASI and sandbox bindings.
    fn create_linker(engine: &Engine) -> std::result::Result<Linker<ExecutorState>, Error> {
        let mut linker = Linker::<ExecutorState>::new(engine);

        // Add WASI support (p2 = preview 2)
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|e| Error::WasmEngine(format!("Failed to add WASI to linker: {e}")))?;

        // Add sandbox bindings
        Sandbox::add_to_linker::<_, HasSelf<ExecutorState>>(&mut linker, |state| state)
            .map_err(|e| Error::WasmEngine(format!("Failed to add sandbox to linker: {e}")))?;

        Ok(linker)
    }

    /// Execute Python code with the given callbacks and trace channel.
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
    /// Returns the captured stdout on success, or an error message on failure.
    pub async fn execute(
        &self,
        code: &str,
        callbacks: &[Arc<dyn Callback>],
        callback_tx: Option<mpsc::Sender<CallbackRequest>>,
        trace_tx: Option<mpsc::UnboundedSender<TraceRequest>>,
    ) -> std::result::Result<String, String> {
        // Build callback info for introspection
        let callback_infos: Vec<HostCallbackInfo> = callbacks
            .iter()
            .map(|cb| HostCallbackInfo {
                name: cb.name().to_string(),
                description: cb.description().to_string(),
                parameters_schema_json: cb.parameters_schema().to_string(),
            })
            .collect();

        // Create WASI context
        let wasi = WasiCtxBuilder::new()
            .inherit_stdout()
            .inherit_stderr()
            .build();

        let state = ExecutorState {
            wasi,
            table: ResourceTable::new(),
            callback_tx,
            trace_tx,
            callbacks: callback_infos,
        };

        // Create store for this execution
        let mut store = Store::new(&self.engine, state);

        // Instantiate the component
        let bindings = Sandbox::instantiate_async(&mut store, &self.component, &self.linker)
            .await
            .map_err(|e| format!("Failed to instantiate component: {e}"))?;

        tracing::debug!(code_len = code.len(), "Executing Python code");

        // Call the async execute export using run_concurrent to get an Accessor
        let code_owned = code.to_string();

        // Call the async execute export
        // run_concurrent returns Result<R, Error> where R is the closure's return type
        // The closure returns wasmtime::Result<Result<String, String>>
        let wasmtime_result = store
            .run_concurrent(async |accessor| bindings.call_execute(accessor, code_owned).await)
            .await
            .map_err(|e| format!("WASM execution error: {e:?}"))?;

        // wasmtime_result is wasmtime::Result<Result<String, String>>
        // Unwrap the outer wasmtime Result

        wasmtime_result.map_err(|e| format!("WASM execution error: {e:?}"))?
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
        #[allow(clippy::match_same_arms)]
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
