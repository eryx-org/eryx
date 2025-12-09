//! Callback trait and error types for host-provided functions.
//!
//! Python code running in the sandbox can call callbacks as direct async
//! functions (e.g., `await get_time()`). The host provides these callbacks
//! by implementing the [`Callback`] trait.

use std::{future::Future, pin::Pin};

/// A callback that Python code can invoke.
///
/// Callbacks are the primary mechanism for Python code to interact
/// with the host environment. They are invoked asynchronously and
/// can perform arbitrary operations (HTTP requests, database queries, etc.).
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{Callback, CallbackError};
/// use serde_json::{json, Value};
/// use std::future::Future;
/// use std::pin::Pin;
///
/// struct GetTime;
///
/// impl Callback for GetTime {
///     fn name(&self) -> &str {
///         "get_time"
///     }
///
///     fn description(&self) -> &str {
///         "Returns the current Unix timestamp"
///     }
///
///     fn parameters_schema(&self) -> Value {
///         json!({
///             "type": "object",
///             "properties": {},
///             "required": []
///         })
///     }
///
///     fn invoke(
///         &self,
///         _args: Value,
///     ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
///         Box::pin(async move {
///             let now = std::time::SystemTime::now()
///                 .duration_since(std::time::UNIX_EPOCH)
///                 .unwrap()
///                 .as_secs();
///             Ok(json!(now))
///         })
///     }
/// }
/// ```
pub trait Callback: Send + Sync {
    /// Unique name for this callback (e.g., "get_time", "echo").
    ///
    /// This name becomes a direct async function in Python:
    /// ```python
    /// result = await get_time()
    /// result = await echo(message="hello")
    /// ```
    ///
    /// For dot-separated names like "http.get", a namespace is created
    /// (unless it conflicts with Python builtins like `math`).
    fn name(&self) -> &str;

    /// Human-readable description of what this callback does.
    ///
    /// This is exposed to Python via `list_callbacks()` for introspection
    /// and can be included in LLM context for code generation.
    fn description(&self) -> &str;

    /// JSON Schema for expected arguments.
    ///
    /// This schema describes the structure of keyword arguments that should be
    /// passed to the callback. It's used for:
    /// - Runtime validation (optional)
    /// - Introspection via `list_callbacks()`
    /// - LLM context for generating correct invocations
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the callback with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `args` - JSON value containing the callback arguments, structured
    ///   according to `parameters_schema()`.
    ///
    /// # Returns
    ///
    /// Returns a JSON value on success, or a [`CallbackError`] on failure.
    /// The return value is serialized and passed back to Python.
    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>;
}

/// Errors that can occur during callback execution.
#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    /// The provided arguments don't match the expected schema.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// The callback execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    /// The requested callback was not found.
    #[error("callback not found: {0}")]
    NotFound(String),

    /// The callback execution timed out.
    #[error("timeout")]
    Timeout,
}
