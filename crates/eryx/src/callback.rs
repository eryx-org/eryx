//! Callback trait and error types for host-provided functions.
//!
//! Python code running in the sandbox can call callbacks as direct async
//! functions (e.g., `await get_time()`). The host provides these callbacks
//! by implementing the [`Callback`] trait.
//!
//! # Choosing a Callback Type
//!
//! This module provides three ways to define callbacks:
//!
//! | Type | Use Case | Schema | Type Safety |
//! |------|----------|--------|-------------|
//! | [`TypedCallback`] | Compile-time known args | Auto-generated | Full |
//! | [`DynamicCallback`] | Runtime-defined callbacks | Runtime-constructed | Partial |
//! | [`Callback`] trait | Maximum flexibility | Manual | Manual |
//!
//! ## Quick Examples
//!
//! **TypedCallback** - Best for most use cases:
//! ```rust,ignore
//! #[derive(Deserialize, JsonSchema)]
//! struct EchoArgs { message: String }
//!
//! struct Echo;
//! impl TypedCallback for Echo {
//!     type Args = EchoArgs;
//!     fn name(&self) -> &str { "echo" }
//!     fn description(&self) -> &str { "Echoes the message" }
//!     fn invoke_typed(&self, args: EchoArgs) -> ... { ... }
//! }
//! ```
//!
//! **DynamicCallback** - For runtime-defined callbacks:
//! ```rust,ignore
//! let callback = DynamicCallback::builder("greet", "Greets a person", |args| {
//!         Box::pin(async move {
//!             let name = args["name"].as_str().unwrap();
//!             Ok(json!({ "greeting": format!("Hello, {}!", name) }))
//!         })
//!     })
//!     .param("name", "string", "The person's name", true)
//!     .param("formal", "boolean", "Use formal greeting", false)
//!     .build();
//! ```
use std::{future::Future, pin::Pin, sync::Arc};

use serde::de::DeserializeOwned;
use serde_json::json;

use crate::schema::{JsonSchema, Schema};

/// A callback that Python code can invoke.
///
/// Callbacks are the primary mechanism for Python code to interact
/// with the host environment. They are invoked asynchronously and
/// can perform arbitrary operations (HTTP requests, database queries, etc.).
///
/// # Choosing Between `Callback` and `TypedCallback`
///
/// - Use [`TypedCallback`] when you have a known argument struct at compile time.
///   It provides automatic schema generation and typed argument parsing.
/// - Use `Callback` directly when you need dynamic/runtime-defined callbacks
///   or when you want full control over schema and argument handling.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{Callback, CallbackError};
/// use schemars::schema::Schema;
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
///     fn parameters_schema(&self) -> Schema {
///         Schema::empty()
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
    ///
    /// Returns a [`Schema`] which provides a strongly-typed
    /// representation of JSON Schema. Use [`Schema::for_type`] to generate
    /// schemas from Rust types, or [`Schema::try_from_value`] for
    /// runtime-defined callbacks.
    fn parameters_schema(&self) -> Schema;

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

/// A strongly-typed callback with compile-time schema generation.
///
/// This trait provides a more ergonomic way to define callbacks when you have
/// a known argument type at compile time. It automatically:
/// - Generates the JSON schema from your argument type via [`schemars::JsonSchema`]
/// - Deserializes arguments into your typed struct
/// - Provides better compile-time guarantees
///
/// Any type implementing `TypedCallback` automatically implements [`Callback`]
/// via a blanket implementation.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{TypedCallback, CallbackError};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
/// use serde_json::{json, Value};
/// use std::future::Future;
/// use std::pin::Pin;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct EchoArgs {
///     /// The message to echo back
///     message: String,
///     /// Number of times to repeat (optional)
///     #[serde(default)]
///     repeat: Option<u32>,
/// }
///
/// struct Echo;
///
/// impl TypedCallback for Echo {
///     type Args = EchoArgs;
///
///     fn name(&self) -> &str {
///         "echo"
///     }
///
///     fn description(&self) -> &str {
///         "Echoes back the provided message"
///     }
///
///     fn invoke_typed(
///         &self,
///         args: EchoArgs,
///     ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
///         Box::pin(async move {
///             let repeat = args.repeat.unwrap_or(1);
///             let repeated: String = std::iter::repeat(args.message.as_str())
///                 .take(repeat as usize)
///                 .collect::<Vec<_>>()
///                 .join(" ");
///             Ok(json!({ "echoed": repeated }))
///         })
///     }
/// }
///
/// // Echo can now be used anywhere a Callback is expected:
/// // let callbacks: Vec<Box<dyn Callback>> = vec![Box::new(Echo)];
/// ```
pub trait TypedCallback: Send + Sync {
    /// The strongly-typed arguments for this callback.
    ///
    /// This type must implement:
    /// - [`serde::de::DeserializeOwned`] for JSON deserialization
    /// - [`schemars::JsonSchema`] for automatic schema generation
    /// - [`Send`] for async safety
    type Args: DeserializeOwned + JsonSchema + Send;

    /// Unique name for this callback.
    ///
    /// See [`Callback::name`] for details.
    fn name(&self) -> &str;

    /// Human-readable description of what this callback does.
    ///
    /// See [`Callback::description`] for details.
    fn description(&self) -> &str;

    /// Execute the callback with typed arguments.
    ///
    /// Unlike [`Callback::invoke`], this method receives already-deserialized
    /// arguments of type `Self::Args`. Deserialization errors are automatically
    /// converted to [`CallbackError::InvalidArguments`].
    fn invoke_typed(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>;
}

/// Blanket implementation: any `TypedCallback` is automatically a `Callback`.
///
/// This allows `TypedCallback` implementors to be used anywhere a `Callback`
/// is expected, including in heterogeneous collections like `Vec<Box<dyn Callback>>`.
impl<T: TypedCallback> Callback for T {
    fn name(&self) -> &str {
        <Self as TypedCallback>::name(self)
    }

    fn description(&self) -> &str {
        <Self as TypedCallback>::description(self)
    }

    fn parameters_schema(&self) -> Schema {
        Schema::for_type::<T::Args>()
    }

    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        // Handle empty objects as null for unit type compatibility.
        // Python sends {} for empty kwargs, but serde expects null for ().
        let args = if args.is_object() && args.as_object().is_some_and(|m| m.is_empty()) {
            serde_json::Value::Null
        } else {
            args
        };

        // Deserialize the JSON value into the typed Args
        let typed_args: Result<T::Args, _> = serde_json::from_value(args);

        Box::pin(async move {
            let args = typed_args.map_err(|e| CallbackError::InvalidArguments(e.to_string()))?;
            self.invoke_typed(args).await
        })
    }
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

/// Helper function to create an empty parameters schema (no arguments).
///
/// This is useful for callbacks that don't take any arguments.
///
/// # Example
///
/// ```rust,ignore
/// fn parameters_schema(&self) -> Schema {
///     eryx::empty_schema()
/// }
/// ```
#[must_use]
pub fn empty_schema() -> Schema {
    Schema::empty()
}

// =============================================================================
// DynamicCallback - Runtime-defined callbacks
// =============================================================================

/// A boxed async handler function for dynamic callbacks.
pub type DynamicHandler = Arc<
    dyn Fn(
            serde_json::Value,
        )
            -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send>>
        + Send
        + Sync,
>;

/// A callback defined entirely at runtime.
///
/// Use this when callback definitions come from configuration, plugins,
/// or are discovered dynamically (e.g., from an API).
///
/// For compile-time type safety, prefer [`TypedCallback`] instead.
///
/// # Example
///
/// ```rust,ignore
/// use eryx::{DynamicCallback, CallbackError};
/// use serde_json::json;
///
/// let greet = DynamicCallback::builder("greet", "Greets a person by name", |args| {
///         Box::pin(async move {
///             let name = args.get("name")
///                 .and_then(|v| v.as_str())
///                 .ok_or_else(|| CallbackError::InvalidArguments("missing 'name'".into()))?;
///
///             let formal = args.get("formal")
///                 .and_then(|v| v.as_bool())
///                 .unwrap_or(false);
///
///             let greeting = if formal {
///                 format!("Good day, {}.", name)
///             } else {
///                 format!("Hey, {}!", name)
///             };
///
///             Ok(json!({ "greeting": greeting }))
///         })
///     })
///     .param("name", "string", "The person's name", true)
///     .param("formal", "boolean", "Use formal greeting", false)
///     .build();
///
/// // Use like any other callback
/// let sandbox = Sandbox::builder()
///     .with_callback(greet)
///     .build()?;
/// ```
///
/// # Schema Construction
///
/// The builder constructs a JSON Schema automatically from the parameter
/// definitions. For more complex schemas, use [`DynamicCallbackBuilder::schema`]
/// to provide a custom schema.
#[derive(Clone)]
pub struct DynamicCallback {
    name: String,
    description: String,
    schema: Schema,
    handler: DynamicHandler,
}

impl std::fmt::Debug for DynamicCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicCallback")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("schema", &self.schema)
            .field("handler", &"<handler>")
            .finish()
    }
}

impl DynamicCallback {
    /// Create a new builder for a dynamic callback.
    ///
    /// # Arguments
    ///
    /// * `name` - The callback name (e.g., "greet", "calculate")
    /// * `description` - Human-readable description
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let callback = DynamicCallback::builder(
    ///         "my_callback",
    ///         "Does something useful",
    ///         |args| Box::pin(async move { Ok(json!({"ok": true})) })
    ///     )
    ///     .param("input", "string", "The input value", true)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder<F>(
        name: impl Into<String>,
        description: impl Into<String>,
        handler: F,
    ) -> DynamicCallbackBuilder
    where
        F: Fn(
                serde_json::Value,
            )
                -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        DynamicCallbackBuilder::new(name, description, handler)
    }

    /// Create a dynamic callback directly with all fields.
    ///
    /// For most cases, prefer using [`DynamicCallback::builder`] instead.
    ///
    /// # Arguments
    ///
    /// * `name` - The callback name
    /// * `description` - Human-readable description
    /// * `schema` - JSON Schema for parameters
    /// * `handler` - The async handler function
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Schema,
        handler: DynamicHandler,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            handler,
        }
    }
}

impl Callback for DynamicCallback {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Schema {
        self.schema.clone()
    }

    fn invoke(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        (self.handler)(args)
    }
}

/// Builder for [`DynamicCallback`].
///
/// Provides a fluent API for constructing runtime-defined callbacks.
///
/// # Example
///
/// ```rust,ignore
/// let callback = DynamicCallback::builder("calculate", "Performs arithmetic", |args| {
///         Box::pin(async move {
///             let op = args["operation"].as_str().unwrap();
///             let a = args["a"].as_f64().unwrap();
///             let b = args["b"].as_f64().unwrap();
///
///             let result = match op {
///                 "add" => a + b,
///                 "sub" => a - b,
///                 "mul" => a * b,
///                 "div" => a / b,
///                 _ => return Err(CallbackError::InvalidArguments(
///                     format!("unknown operation: {}", op)
///                 )),
///             };
///
///             Ok(json!({ "result": result }))
///         })
///     })
///     .param("operation", "string", "The operation: add, sub, mul, div", true)
///     .param("a", "number", "First operand", true)
///     .param("b", "number", "Second operand", true)
///     .build();
/// ```
pub struct DynamicCallbackBuilder {
    name: String,
    description: String,
    parameters: Vec<ParameterDef>,
    custom_schema: Option<Schema>,
    handler: DynamicHandler,
}

impl std::fmt::Debug for DynamicCallbackBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicCallbackBuilder")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("custom_schema", &self.custom_schema)
            .field("handler", &"<handler>")
            .finish()
    }
}

/// Definition of a single parameter for schema generation.
#[derive(Debug, Clone)]
struct ParameterDef {
    name: String,
    json_type: String,
    description: String,
    required: bool,
}

impl DynamicCallbackBuilder {
    /// Create a new builder with the required handler.
    fn new<F>(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self
    where
        F: Fn(
                serde_json::Value,
            )
                -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: Vec::new(),
            custom_schema: None,
            handler: Arc::new(handler),
        }
    }

    /// Add a parameter to the callback's schema.
    ///
    /// # Arguments
    ///
    /// * `name` - Parameter name (e.g., "message", "count")
    /// * `json_type` - JSON Schema type: "string", "number", "integer", "boolean", "object", "array"
    /// * `description` - Human-readable description of the parameter
    /// * `required` - Whether this parameter is required
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// DynamicCallback::builder("example", "An example callback")
    ///     .param("name", "string", "The user's name", true)
    ///     .param("age", "integer", "The user's age", false)
    ///     .param("tags", "array", "Optional tags", false)
    /// ```
    #[must_use]
    pub fn param(
        mut self,
        name: impl Into<String>,
        json_type: impl Into<String>,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        self.parameters.push(ParameterDef {
            name: name.into(),
            json_type: json_type.into(),
            description: description.into(),
            required,
        });
        self
    }

    /// Set a custom JSON Schema instead of building one from parameters.
    ///
    /// Use this when you need more control over the schema than [`param`](Self::param) provides,
    /// such as for complex nested objects, enums, or validation constraints.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::schemars::json_schema;
    ///
    /// DynamicCallback::builder("complex", "A callback with complex schema", handler)
    ///     .schema(Schema::try_from_value(json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "mode": {
    ///                 "type": "string",
    ///                 "enum": ["fast", "slow", "auto"]
    ///             },
    ///             "config": {
    ///                 "type": "object",
    ///                 "additionalProperties": true
    ///             }
    ///         },
    ///         "required": ["mode"]
    ///     })).unwrap())
    /// ```
    #[must_use]
    pub fn schema(mut self, schema: Schema) -> Self {
        self.custom_schema = Some(schema);
        self
    }

    /// Build the [`DynamicCallback`].
    #[must_use]
    pub fn build(self) -> DynamicCallback {
        // Build schema first while we can still borrow self
        let schema = match self.custom_schema {
            Some(s) => s,
            None => {
                // Build from parameters
                if self.parameters.is_empty() {
                    Schema::empty()
                } else {
                    let mut properties = serde_json::Map::new();
                    let mut required = Vec::new();

                    for param in &self.parameters {
                        properties.insert(
                            param.name.clone(),
                            json!({
                                "type": param.json_type,
                                "description": param.description
                            }),
                        );
                        if param.required {
                            required.push(serde_json::Value::String(param.name.clone()));
                        }
                    }

                    let schema_json = json!({
                        "type": "object",
                        "properties": properties,
                        "required": required
                    });

                    Schema::try_from_value(schema_json).unwrap_or_default()
                }
            }
        };

        DynamicCallback {
            name: self.name,
            description: self.description,
            schema,
            handler: self.handler,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    // ==========================================================================
    // CallbackError tests
    // ==========================================================================

    #[test]
    fn callback_error_invalid_arguments_displays_message() {
        let error = CallbackError::InvalidArguments("missing field 'name'".to_string());
        let display = format!("{}", error);
        assert!(display.contains("invalid arguments"));
        assert!(display.contains("missing field 'name'"));
    }

    #[test]
    fn callback_error_execution_failed_displays_message() {
        let error = CallbackError::ExecutionFailed("connection timeout".to_string());
        let display = format!("{}", error);
        assert!(display.contains("execution failed"));
        assert!(display.contains("connection timeout"));
    }

    #[test]
    fn callback_error_not_found_displays_name() {
        let error = CallbackError::NotFound("unknown_callback".to_string());
        let display = format!("{}", error);
        assert!(display.contains("not found"));
        assert!(display.contains("unknown_callback"));
    }

    #[test]
    fn callback_error_timeout_displays_correctly() {
        let error = CallbackError::Timeout;
        let display = format!("{}", error);
        assert!(display.contains("timeout"));
    }

    #[test]
    fn callback_error_is_debug() {
        let error = CallbackError::InvalidArguments("test".to_string());
        let debug = format!("{:?}", error);
        assert!(debug.contains("InvalidArguments"));
    }

    // ==========================================================================
    // TypedCallback tests
    // ==========================================================================

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        message: String,
    }

    struct EchoCallback;

    impl TypedCallback for EchoCallback {
        type Args = EchoArgs;

        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes the message back"
        }

        fn invoke_typed(
            &self,
            args: EchoArgs,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>
        {
            Box::pin(async move { Ok(json!({ "echoed": args.message })) })
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    struct AddCallback;

    impl TypedCallback for AddCallback {
        type Args = AddArgs;

        fn name(&self) -> &str {
            "add"
        }

        fn description(&self) -> &str {
            "Adds two numbers"
        }

        fn invoke_typed(
            &self,
            args: AddArgs,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>
        {
            Box::pin(async move { Ok(json!(args.a + args.b)) })
        }
    }

    struct NoArgsCallback;

    impl TypedCallback for NoArgsCallback {
        type Args = ();

        fn name(&self) -> &str {
            "no_args"
        }

        fn description(&self) -> &str {
            "A callback with no arguments"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>
        {
            Box::pin(async move { Ok(json!("success")) })
        }
    }

    struct FailingCallback;

    impl TypedCallback for FailingCallback {
        type Args = ();

        fn name(&self) -> &str {
            "fail"
        }

        fn description(&self) -> &str {
            "Always fails"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>>
        {
            Box::pin(
                async move { Err(CallbackError::ExecutionFailed("intentional failure".into())) },
            )
        }
    }

    #[test]
    fn typed_callback_name_and_description() {
        let callback = EchoCallback;
        assert_eq!(TypedCallback::name(&callback), "echo");
        assert_eq!(
            TypedCallback::description(&callback),
            "Echoes the message back"
        );
    }

    #[test]
    fn typed_callback_generates_schema() {
        let callback = EchoCallback;
        // Access via Callback trait (blanket impl)
        let schema = Callback::parameters_schema(&callback);
        let value = schema.to_value();

        // Should have properties with message field
        let properties = value.get("properties").expect("should have properties");
        assert!(properties.get("message").is_some());
    }

    #[test]
    fn typed_callback_no_args_generates_empty_schema() {
        let callback = NoArgsCallback;
        let schema = Callback::parameters_schema(&callback);
        let value = schema.to_value();

        // Unit type schema should be valid (either object or boolean in JSON Schema)
        assert!(value.is_object() || value.is_boolean());
    }

    #[tokio::test]
    async fn typed_callback_invoke_with_valid_args() {
        let callback = EchoCallback;
        let args = json!({ "message": "hello world" });

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_ok());

        let value = result.unwrap();
        assert_eq!(value["echoed"], "hello world");
    }

    #[tokio::test]
    async fn typed_callback_invoke_with_numeric_args() {
        let callback = AddCallback;
        let args = json!({ "a": 10, "b": 32 });

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!(42));
    }

    #[tokio::test]
    async fn typed_callback_invoke_with_null_for_unit() {
        let callback = NoArgsCallback;
        // Unit type deserializes from null
        let args = serde_json::Value::Null;

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!("success"));
    }

    #[tokio::test]
    async fn typed_callback_invoke_with_missing_required_field() {
        let callback = EchoCallback;
        let args = json!({}); // Missing "message" field

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert!(matches!(error, CallbackError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn typed_callback_invoke_with_wrong_type() {
        let callback = EchoCallback;
        let args = json!({ "message": 12345 }); // Number instead of string

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert!(matches!(error, CallbackError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn typed_callback_can_return_error() {
        let callback = FailingCallback;
        // Unit type needs null, not empty object
        let args = serde_json::Value::Null;

        let result = Callback::invoke(&callback, args).await;
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert!(matches!(error, CallbackError::ExecutionFailed(_)));
    }

    #[test]
    fn typed_callback_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EchoCallback>();
        assert_send_sync::<NoArgsCallback>();
    }

    // ==========================================================================
    // DynamicCallback tests
    // ==========================================================================

    #[test]
    fn dynamic_callback_builder_basic() {
        let callback = DynamicCallback::builder("test", "A test callback", |_args| {
            Box::pin(async move { Ok(json!({"ok": true})) })
        })
        .build();

        assert_eq!(callback.name(), "test");
        assert_eq!(callback.description(), "A test callback");
    }

    #[test]
    fn dynamic_callback_builder_with_params() {
        let callback = DynamicCallback::builder("greet", "Greets someone", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .param("name", "string", "The person's name", true)
        .param("formal", "boolean", "Use formal greeting", false)
        .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();

        // Check properties exist
        let properties = value.get("properties").expect("should have properties");
        assert!(properties.get("name").is_some());
        assert!(properties.get("formal").is_some());

        // Check required array
        let required = value.get("required").expect("should have required");
        let required_arr = required.as_array().expect("required should be array");
        assert!(required_arr.contains(&json!("name")));
        assert!(!required_arr.contains(&json!("formal")));
    }

    #[test]
    fn dynamic_callback_builder_no_params_empty_schema() {
        let callback = DynamicCallback::builder("simple", "No params", |_args| {
            Box::pin(async move { Ok(json!(null)) })
        })
        .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();

        // Empty schema should be valid
        assert!(value.is_object() || value.is_boolean());
    }

    #[test]
    fn dynamic_callback_builder_with_custom_schema() {
        let custom_schema = Schema::try_from_value(json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["fast", "slow"]
                }
            },
            "required": ["mode"]
        }))
        .unwrap();

        let callback = DynamicCallback::builder("custom", "Custom schema", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .schema(custom_schema)
        .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();

        let properties = value.get("properties").unwrap();
        let mode = properties.get("mode").unwrap();
        assert!(mode.get("enum").is_some());
    }

    #[test]
    fn dynamic_callback_custom_schema_overrides_params() {
        let custom_schema = Schema::try_from_value(json!({
            "type": "object",
            "properties": {
                "override": { "type": "string" }
            }
        }))
        .unwrap();

        let callback = DynamicCallback::builder("test", "Test", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .param("ignored", "string", "This should be ignored", true)
        .schema(custom_schema)
        .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();

        let properties = value.get("properties").unwrap();
        assert!(properties.get("override").is_some());
        assert!(properties.get("ignored").is_none());
    }

    #[tokio::test]
    async fn dynamic_callback_invoke_success() {
        let callback = DynamicCallback::builder("echo", "Echo", |args| {
            Box::pin(async move {
                let msg = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                Ok(json!({ "echoed": msg }))
            })
        })
        .param("message", "string", "Message to echo", true)
        .build();

        let result = callback.invoke(json!({ "message": "hello" })).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["echoed"], "hello");
    }

    #[tokio::test]
    async fn dynamic_callback_invoke_can_fail() {
        let callback = DynamicCallback::builder("fail", "Always fails", |_args| {
            Box::pin(async move { Err(CallbackError::ExecutionFailed("boom".into())) })
        })
        .build();

        let result = callback.invoke(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dynamic_callback_invoke_with_validation() {
        let callback = DynamicCallback::builder("validate", "Validates input", |args| {
            Box::pin(async move {
                let value = args
                    .get("value")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| CallbackError::InvalidArguments("missing 'value'".into()))?;

                if value < 0 {
                    return Err(CallbackError::InvalidArguments("value must be >= 0".into()));
                }

                Ok(json!({ "validated": value }))
            })
        })
        .param("value", "integer", "Value to validate", true)
        .build();

        // Valid input
        let result = callback.invoke(json!({ "value": 42 })).await;
        assert!(result.is_ok());

        // Invalid input (negative)
        let result = callback.invoke(json!({ "value": -5 })).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CallbackError::InvalidArguments(_)
        ));

        // Missing input
        let result = callback.invoke(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn dynamic_callback_is_clone() {
        let callback = DynamicCallback::builder("test", "Test", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .build();

        let cloned = callback.clone();
        assert_eq!(cloned.name(), callback.name());
    }

    #[test]
    fn dynamic_callback_is_debug() {
        let callback = DynamicCallback::builder("test", "Test callback", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .build();

        let debug = format!("{:?}", callback);
        assert!(debug.contains("DynamicCallback"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn dynamic_callback_builder_is_debug() {
        let builder = DynamicCallback::builder("test", "Test", |_args| {
            Box::pin(async move { Ok(json!({})) })
        })
        .param("x", "string", "A param", true);

        let debug = format!("{:?}", builder);
        assert!(debug.contains("DynamicCallbackBuilder"));
    }

    #[test]
    fn dynamic_callback_new_direct_construction() {
        let handler: DynamicHandler = Arc::new(|_args| Box::pin(async move { Ok(json!({})) }));

        let callback =
            DynamicCallback::new("direct", "Directly constructed", Schema::empty(), handler);

        assert_eq!(callback.name(), "direct");
        assert_eq!(callback.description(), "Directly constructed");
    }

    // ==========================================================================
    // empty_schema helper tests
    // ==========================================================================

    #[test]
    fn empty_schema_returns_valid_schema() {
        let schema = empty_schema();
        let value = schema.to_value();
        assert!(value.is_object() || value.is_boolean());
    }

    // ==========================================================================
    // Callback trait object tests
    // ==========================================================================

    #[test]
    fn callbacks_can_be_boxed_as_trait_objects() {
        let typed: Box<dyn Callback> = Box::new(EchoCallback);
        let dynamic: Box<dyn Callback> = Box::new(
            DynamicCallback::builder("dyn", "Dynamic", |_| Box::pin(async { Ok(json!({})) }))
                .build(),
        );

        // Can put them in the same collection
        let callbacks: Vec<Box<dyn Callback>> = vec![typed, dynamic];
        assert_eq!(callbacks.len(), 2);
        assert_eq!(callbacks[0].name(), "echo");
        assert_eq!(callbacks[1].name(), "dyn");
    }

    #[tokio::test]
    async fn callback_trait_object_invoke() {
        let callback: Box<dyn Callback> = Box::new(AddCallback);

        let result = callback.invoke(json!({ "a": 5, "b": 7 })).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!(12));
    }

    // ==========================================================================
    // Parameter type tests for DynamicCallback
    // ==========================================================================

    #[test]
    fn dynamic_callback_supports_all_json_types() {
        let callback =
            DynamicCallback::builder("types", "All types", |_| Box::pin(async { Ok(json!({})) }))
                .param("str_param", "string", "A string", true)
                .param("num_param", "number", "A number", true)
                .param("int_param", "integer", "An integer", true)
                .param("bool_param", "boolean", "A boolean", true)
                .param("obj_param", "object", "An object", false)
                .param("arr_param", "array", "An array", false)
                .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();
        let properties = value.get("properties").unwrap();

        assert_eq!(properties["str_param"]["type"], "string");
        assert_eq!(properties["num_param"]["type"], "number");
        assert_eq!(properties["int_param"]["type"], "integer");
        assert_eq!(properties["bool_param"]["type"], "boolean");
        assert_eq!(properties["obj_param"]["type"], "object");
        assert_eq!(properties["arr_param"]["type"], "array");
    }

    #[test]
    fn dynamic_callback_param_descriptions_preserved() {
        let callback = DynamicCallback::builder("desc", "Test descriptions", |_| {
            Box::pin(async { Ok(json!({})) })
        })
        .param("field", "string", "This is a detailed description", true)
        .build();

        let schema = callback.parameters_schema();
        let value = schema.to_value();
        let properties = value.get("properties").unwrap();

        assert_eq!(
            properties["field"]["description"],
            "This is a detailed description"
        );
    }
}
