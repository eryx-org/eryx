//! JSON Schema types for callback parameter definitions.
//!
//! This module provides an opaque [`Schema`] type and re-exports the [`JsonSchema`]
//! derive macro. By using these types instead of `schemars` directly, your code
//! is insulated from changes to the underlying schema library.
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx::schema::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct MyArgs {
//!     /// The message to process
//!     message: String,
//!     /// Optional repeat count
//!     #[serde(default)]
//!     repeat: Option<u32>,
//! }
//! ```

use serde::Serialize;

/// Re-export the `JsonSchema` derive macro.
///
/// Use this to automatically generate JSON Schema for your callback argument types:
///
/// ```rust,ignore
/// use eryx::schema::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct EchoArgs {
///     message: String,
/// }
/// ```
///
/// The generated schema will be used by [`TypedCallback`](crate::TypedCallback)
/// to provide introspection and validation information.
pub use schemars::JsonSchema;

/// An opaque JSON Schema type.
///
/// This wraps the underlying schema implementation, allowing the library
/// to change schema backends without breaking the public API.
///
/// # Creating Schemas
///
/// Schemas are typically created automatically via [`TypedCallback`](crate::TypedCallback),
/// but you can also create them manually:
///
/// ```rust,ignore
/// use eryx::schema::{Schema, JsonSchema};
///
/// // From a type implementing JsonSchema
/// let schema = Schema::for_type::<MyArgs>();
///
/// // From a JSON value (for runtime-defined schemas)
/// let schema = Schema::try_from_value(json!({
///     "type": "object",
///     "properties": {
///         "name": { "type": "string" }
///     }
/// }))?;
///
/// // Empty schema (for callbacks with no arguments)
/// let schema = Schema::empty();
/// ```
#[derive(Clone, Debug)]
pub struct Schema(pub(crate) schemars::Schema);

impl Schema {
    /// Create a schema for a type that implements [`JsonSchema`].
    ///
    /// This is the primary way to create schemas for typed callbacks.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::schema::{Schema, JsonSchema};
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Args { value: i32 }
    ///
    /// let schema = Schema::for_type::<Args>();
    /// ```
    #[must_use]
    pub fn for_type<T: JsonSchema>() -> Self {
        Self(schemars::SchemaGenerator::default().into_root_schema_for::<T>())
    }

    /// Create an empty schema (for callbacks with no arguments).
    ///
    /// Equivalent to `Schema::for_type::<()>()`.
    #[must_use]
    pub fn empty() -> Self {
        Self::for_type::<()>()
    }

    /// Try to create a schema from a JSON value.
    ///
    /// Use this for runtime-defined schemas where you have the schema
    /// as a JSON object (e.g., from a configuration file or API).
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON value is not a valid JSON Schema.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use eryx::schema::Schema;
    /// use serde_json::json;
    ///
    /// let schema = Schema::try_from_value(json!({
    ///     "type": "object",
    ///     "properties": {
    ///         "name": { "type": "string" }
    ///     },
    ///     "required": ["name"]
    /// }))?;
    /// ```
    pub fn try_from_value(value: serde_json::Value) -> Result<Self, SchemaError> {
        schemars::Schema::try_from(value)
            .map(Self)
            .map_err(|e| SchemaError::InvalidSchema(e.to_string()))
    }

    /// Convert the schema to a JSON value.
    ///
    /// Useful for serialization or introspection.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or_default()
    }

    /// Serialize the schema to a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (should not happen for valid schemas).
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.0)
    }

    /// Serialize the schema to a pretty-printed JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (should not happen for valid schemas).
    pub fn to_json_string_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.0)
    }
}

impl Serialize for Schema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl From<schemars::Schema> for Schema {
    fn from(schema: schemars::Schema) -> Self {
        Self(schema)
    }
}

impl Default for Schema {
    fn default() -> Self {
        Self::empty()
    }
}

/// Errors that can occur when working with schemas.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The provided JSON value is not a valid JSON Schema.
    #[error("invalid schema: {0}")]
    InvalidSchema(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct TestArgs {
        name: String,
        #[serde(default)]
        count: Option<i32>,
    }

    #[test]
    fn test_schema_for_type() {
        let schema = Schema::for_type::<TestArgs>();
        let value = schema.to_value();

        // Should have properties
        assert!(value.get("properties").is_some());
        let props = value.get("properties").unwrap();
        assert!(props.get("name").is_some());
        assert!(props.get("count").is_some());
    }

    #[test]
    fn test_schema_empty() {
        let schema = Schema::empty();
        let value = schema.to_value();

        // Unit type schema - schemars represents () as a null schema or similar
        assert!(value.is_object() || value.is_boolean());
    }

    #[test]
    fn test_schema_try_from_value() {
        let schema = Schema::try_from_value(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            }
        }));

        assert!(schema.is_ok());
    }

    #[test]
    fn test_schema_to_json_string() {
        let schema = Schema::for_type::<TestArgs>();
        let json_str = schema.to_json_string();

        assert!(json_str.is_ok());
        assert!(json_str.unwrap().contains("properties"));
    }

    #[test]
    fn test_schema_default() {
        let schema = Schema::default();
        // Default should be empty schema
        let _ = schema.to_value();
    }
}
