//! Error types for the Eryx sandbox.

use crate::callback::CallbackError;

/// The main error type for Eryx operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error during sandbox initialization.
    #[error("initialization failed: {0}")]
    Initialization(String),

    /// Error during WASM engine creation.
    #[error("wasm engine error: {0}")]
    WasmEngine(String),

    /// Error during WASM component loading or instantiation.
    #[error("wasm component error: {0}")]
    WasmComponent(#[from] wasmtime::Error),

    /// Error during Python execution.
    #[error("execution failed: {0}")]
    Execution(String),

    /// A callback error occurred during execution.
    #[error("callback error: {0}")]
    Callback(#[from] CallbackError),

    /// Resource limit exceeded.
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),

    /// Execution timed out.
    #[error("execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Serialization/deserialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Python stdlib not found during auto-detection.
    ///
    /// Use [`SandboxBuilder::with_python_stdlib()`](crate::SandboxBuilder::with_python_stdlib)
    /// to specify the stdlib path explicitly, or enable the `embedded` feature and use
    /// [`Sandbox::embedded()`](crate::Sandbox::embedded).
    #[error(
        "Python stdlib not found. Set ERYX_PYTHON_STDLIB, use with_python_stdlib(), or use Sandbox::embedded()"
    )]
    MissingPythonStdlib,

    /// State snapshot error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}
