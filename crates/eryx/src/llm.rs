//! LLM tool-calling integration for exporting callbacks as LLM tool schemas.
//!
//! This module provides utilities for integrating Eryx callbacks with LLM tool-calling
//! APIs from providers like OpenAI and Anthropic.
//!
//! # Overview
//!
//! LLMs can be given access to "tools" (functions) that they can invoke during
//! conversations. Different providers use different JSON formats to describe these
//! tools. This module handles:
//!
//! 1. **Schema Export** - Convert Eryx callbacks to LLM-compatible tool definitions
//! 2. **Tool Call Execution** - Parse and execute tool calls from LLM responses
//!
//! # Supported Formats
//!
//! - [`ToolFormat::OpenAI`] - OpenAI's function calling format
//! - [`ToolFormat::Anthropic`] - Anthropic's tool_use format
//! - [`ToolFormat::Generic`] - Raw JSON Schema (provider-agnostic)

use std::sync::Arc;

use serde_json::{json, Value};

use crate::callback::{Callback, CallbackError};

/// The format to use for tool schema export and tool call parsing.
///
/// Different LLM providers use different JSON structures for tool definitions
/// and tool call responses. This enum specifies which format to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolFormat {
    /// OpenAI function calling format.
    #[default]
    OpenAI,

    /// Anthropic tool_use format.
    Anthropic,

    /// Generic JSON Schema format (provider-agnostic).
    Generic,
}

impl std::fmt::Display for ToolFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolFormat::OpenAI => write!(f, "OpenAI"),
            ToolFormat::Anthropic => write!(f, "Anthropic"),
            ToolFormat::Generic => write!(f, "Generic"),
        }
    }
}

/// Export a single callback as a tool schema in the specified format.
fn export_callback_schema(callback: &dyn Callback, format: ToolFormat) -> Value {
    let name = callback.name();
    let description = callback.description();
    let parameters = callback.parameters_schema().to_value();

    match format {
        ToolFormat::OpenAI => {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            })
        }
        ToolFormat::Anthropic => {
            json!({
                "name": name,
                "description": description,
                "input_schema": parameters
            })
        }
        ToolFormat::Generic => {
            json!({
                "name": name,
                "description": description,
                "parameters": parameters
            })
        }
    }
}

/// Export multiple callbacks as tool schemas in the specified format.
///
/// Returns a JSON array of tool definitions.
pub fn export_tool_schemas<'a>(
    callbacks: impl IntoIterator<Item = &'a Arc<dyn Callback>>,
    format: ToolFormat,
) -> Value {
    let tools: Vec<Value> = callbacks
        .into_iter()
        .map(|cb| export_callback_schema(cb.as_ref(), format))
        .collect();
    Value::Array(tools)
}

/// A parsed tool call from an LLM response.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// The tool call ID (for correlating responses).
    pub id: Option<String>,
    /// The name of the tool/callback to invoke.
    pub name: String,
    /// The arguments to pass to the callback.
    pub arguments: Value,
}

/// Parse a tool call from an LLM response in the specified format.
///
/// # Errors
///
/// Returns an error if the tool call JSON does not match the expected format.
pub fn parse_tool_call(tool_call: &Value, format: ToolFormat) -> Result<ParsedToolCall, ToolCallError> {
    match format {
        ToolFormat::OpenAI => parse_openai_tool_call(tool_call),
        ToolFormat::Anthropic => parse_anthropic_tool_call(tool_call),
        ToolFormat::Generic => parse_generic_tool_call(tool_call),
    }
}

/// Parse an OpenAI format tool call.
fn parse_openai_tool_call(tool_call: &Value) -> Result<ParsedToolCall, ToolCallError> {
    let id = tool_call.get("id").and_then(|v| v.as_str()).map(String::from);

    let function = tool_call
        .get("function")
        .ok_or_else(|| ToolCallError::MissingField("function".to_string()))?;

    let name = function
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolCallError::MissingField("function.name".to_string()))?
        .to_string();

    let arguments_raw = function
        .get("arguments")
        .ok_or_else(|| ToolCallError::MissingField("function.arguments".to_string()))?;

    let arguments = if let Some(args_str) = arguments_raw.as_str() {
        serde_json::from_str(args_str).map_err(|e| {
            ToolCallError::InvalidArguments(format!("failed to parse arguments JSON: {e}"))
        })?
    } else {
        arguments_raw.clone()
    };

    Ok(ParsedToolCall { id, name, arguments })
}

/// Parse an Anthropic format tool call.
fn parse_anthropic_tool_call(tool_call: &Value) -> Result<ParsedToolCall, ToolCallError> {
    let id = tool_call.get("id").and_then(|v| v.as_str()).map(String::from);

    let name = tool_call
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolCallError::MissingField("name".to_string()))?
        .to_string();

    let arguments = tool_call
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    Ok(ParsedToolCall { id, name, arguments })
}

/// Parse a generic format tool call.
fn parse_generic_tool_call(tool_call: &Value) -> Result<ParsedToolCall, ToolCallError> {
    let id = tool_call.get("id").and_then(|v| v.as_str()).map(String::from);

    let name = tool_call
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolCallError::MissingField("name".to_string()))?
        .to_string();

    let arguments = tool_call
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    Ok(ParsedToolCall { id, name, arguments })
}

/// Format a tool call result for the specified LLM format.
pub fn format_tool_result(
    tool_call_id: Option<&str>,
    tool_name: &str,
    result: Result<Value, CallbackError>,
    format: ToolFormat,
) -> Value {
    match format {
        ToolFormat::OpenAI => {
            let content = match result {
                Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
                Err(e) => json!({"error": e.to_string()}).to_string(),
            };
            json!({
                "role": "tool",
                "tool_call_id": tool_call_id.unwrap_or(""),
                "content": content
            })
        }
        ToolFormat::Anthropic => {
            let (content, is_error) = match result {
                Ok(value) => (value, false),
                Err(e) => (json!({"error": e.to_string()}), true),
            };
            json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id.unwrap_or(""),
                "content": content,
                "is_error": is_error
            })
        }
        ToolFormat::Generic => {
            match result {
                Ok(value) => json!({
                    "name": tool_name,
                    "result": value
                }),
                Err(e) => json!({
                    "name": tool_name,
                    "error": e.to_string()
                }),
            }
        }
    }
}

/// Errors that can occur when parsing or executing tool calls.
#[derive(Debug, thiserror::Error)]
pub enum ToolCallError {
    /// A required field is missing from the tool call JSON.
    #[error("missing required field: {0}")]
    MissingField(String),

    /// The arguments could not be parsed.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// The requested tool/callback was not found.
    #[error("tool not found: {0}")]
    ToolNotFound(String),

    /// The callback execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(#[from] CallbackError),
}


#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::callback::{DynamicCallback, TypedCallback};
    use crate::schema::JsonSchema;
    use serde::Deserialize;
    use std::future::Future;
    use std::pin::Pin;

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
            "Echoes back the provided message"
        }

        fn invoke_typed(
            &self,
            args: EchoArgs,
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!({"echoed": args.message})) })
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
            "Adds two numbers together"
        }

        fn invoke_typed(
            &self,
            args: AddArgs,
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!(args.a + args.b)) })
        }
    }

    #[test]
    fn tool_format_display() {
        assert_eq!(format!("{}", ToolFormat::OpenAI), "OpenAI");
        assert_eq!(format!("{}", ToolFormat::Anthropic), "Anthropic");
        assert_eq!(format!("{}", ToolFormat::Generic), "Generic");
    }

    #[test]
    fn tool_format_default_is_openai() {
        assert_eq!(ToolFormat::default(), ToolFormat::OpenAI);
    }

    #[test]
    fn export_openai_format_single_callback() {
        let callback: Arc<dyn Callback> = Arc::new(EchoCallback);
        let callbacks = vec![callback];
        let tools = export_tool_schemas(&callbacks, ToolFormat::OpenAI);

        let tools_arr = tools.as_array().expect("should be array");
        assert_eq!(tools_arr.len(), 1);

        let tool = &tools_arr[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "echo");
        assert_eq!(tool["function"]["description"], "Echoes back the provided message");
        assert!(tool["function"]["parameters"].is_object());
    }

    #[test]
    fn export_openai_format_multiple_callbacks() {
        let callbacks: Vec<Arc<dyn Callback>> = vec![Arc::new(EchoCallback), Arc::new(AddCallback)];
        let tools = export_tool_schemas(&callbacks, ToolFormat::OpenAI);

        let tools_arr = tools.as_array().expect("should be array");
        assert_eq!(tools_arr.len(), 2);

        let names: Vec<&str> = tools_arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"add"));
    }

    #[test]
    fn export_anthropic_format_single_callback() {
        let callback: Arc<dyn Callback> = Arc::new(EchoCallback);
        let callbacks = vec![callback];
        let tools = export_tool_schemas(&callbacks, ToolFormat::Anthropic);

        let tools_arr = tools.as_array().expect("should be array");
        assert_eq!(tools_arr.len(), 1);

        let tool = &tools_arr[0];
        assert_eq!(tool["name"], "echo");
        assert_eq!(tool["description"], "Echoes back the provided message");
        assert!(tool["input_schema"].is_object());
        assert!(tool.get("type").is_none());
    }

    #[test]
    fn export_generic_format_single_callback() {
        let callback: Arc<dyn Callback> = Arc::new(EchoCallback);
        let callbacks = vec![callback];
        let tools = export_tool_schemas(&callbacks, ToolFormat::Generic);

        let tools_arr = tools.as_array().expect("should be array");
        assert_eq!(tools_arr.len(), 1);

        let tool = &tools_arr[0];
        assert_eq!(tool["name"], "echo");
        assert_eq!(tool["description"], "Echoes back the provided message");
        assert!(tool["parameters"].is_object());
    }

    #[test]
    fn parse_openai_tool_call_success() {
        let tool_call = json!({
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "echo",
                "arguments": "{\"message\": \"hello\"}"
            }
        });

        let parsed = parse_tool_call(&tool_call, ToolFormat::OpenAI).unwrap();

        assert_eq!(parsed.id, Some("call_abc123".to_string()));
        assert_eq!(parsed.name, "echo");
        assert_eq!(parsed.arguments["message"], "hello");
    }

    #[test]
    fn parse_openai_tool_call_arguments_as_object() {
        let tool_call = json!({
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "add",
                "arguments": {"a": 1, "b": 2}
            }
        });

        let parsed = parse_tool_call(&tool_call, ToolFormat::OpenAI).unwrap();

        assert_eq!(parsed.name, "add");
        assert_eq!(parsed.arguments["a"], 1);
        assert_eq!(parsed.arguments["b"], 2);
    }

    #[test]
    fn parse_openai_tool_call_missing_function() {
        let tool_call = json!({
            "id": "call_abc123",
            "type": "function"
        });

        let result = parse_tool_call(&tool_call, ToolFormat::OpenAI);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolCallError::MissingField(_)));
    }

    #[test]
    fn parse_openai_tool_call_invalid_json_arguments() {
        let tool_call = json!({
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "echo",
                "arguments": "not valid json"
            }
        });

        let result = parse_tool_call(&tool_call, ToolFormat::OpenAI);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolCallError::InvalidArguments(_)));
    }

    #[test]
    fn parse_anthropic_tool_call_success() {
        let tool_call = json!({
            "type": "tool_use",
            "id": "toolu_abc123",
            "name": "echo",
            "input": {"message": "hello"}
        });

        let parsed = parse_tool_call(&tool_call, ToolFormat::Anthropic).unwrap();

        assert_eq!(parsed.id, Some("toolu_abc123".to_string()));
        assert_eq!(parsed.name, "echo");
        assert_eq!(parsed.arguments["message"], "hello");
    }

    #[test]
    fn parse_anthropic_tool_call_empty_input() {
        let tool_call = json!({
            "type": "tool_use",
            "id": "toolu_abc123",
            "name": "no_args"
        });

        let parsed = parse_tool_call(&tool_call, ToolFormat::Anthropic).unwrap();

        assert_eq!(parsed.name, "no_args");
        assert!(parsed.arguments.is_object());
    }

    #[test]
    fn parse_generic_tool_call_success() {
        let tool_call = json!({
            "name": "echo",
            "arguments": {"message": "hello"}
        });

        let parsed = parse_tool_call(&tool_call, ToolFormat::Generic).unwrap();

        assert!(parsed.id.is_none());
        assert_eq!(parsed.name, "echo");
        assert_eq!(parsed.arguments["message"], "hello");
    }

    #[test]
    fn format_tool_result_openai_success() {
        let result = format_tool_result(
            Some("call_abc123"),
            "echo",
            Ok(json!({"echoed": "hello"})),
            ToolFormat::OpenAI,
        );

        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], "call_abc123");
        let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["echoed"], "hello");
    }

    #[test]
    fn format_tool_result_openai_error() {
        let result = format_tool_result(
            Some("call_abc123"),
            "echo",
            Err(CallbackError::ExecutionFailed("test error".to_string())),
            ToolFormat::OpenAI,
        );

        assert_eq!(result["role"], "tool");
        let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert!(content["error"].as_str().unwrap().contains("test error"));
    }

    #[test]
    fn format_tool_result_anthropic_success() {
        let result = format_tool_result(
            Some("toolu_abc123"),
            "echo",
            Ok(json!({"echoed": "hello"})),
            ToolFormat::Anthropic,
        );

        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "toolu_abc123");
        assert_eq!(result["content"]["echoed"], "hello");
        assert_eq!(result["is_error"], false);
    }

    #[test]
    fn format_tool_result_anthropic_error() {
        let result = format_tool_result(
            Some("toolu_abc123"),
            "echo",
            Err(CallbackError::ExecutionFailed("test error".to_string())),
            ToolFormat::Anthropic,
        );

        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["is_error"], true);
        assert!(result["content"]["error"].as_str().unwrap().contains("test error"));
    }

    #[test]
    fn format_tool_result_generic_success() {
        let result = format_tool_result(
            None,
            "echo",
            Ok(json!({"echoed": "hello"})),
            ToolFormat::Generic,
        );

        assert_eq!(result["name"], "echo");
        assert_eq!(result["result"]["echoed"], "hello");
        assert!(result.get("error").is_none());
    }

    #[test]
    fn format_tool_result_generic_error() {
        let result = format_tool_result(
            None,
            "echo",
            Err(CallbackError::ExecutionFailed("test error".to_string())),
            ToolFormat::Generic,
        );

        assert_eq!(result["name"], "echo");
        assert!(result.get("result").is_none());
        assert!(result["error"].as_str().unwrap().contains("test error"));
    }

    #[test]
    fn export_dynamic_callback_schema() {
        let callback = DynamicCallback::builder("greet", "Greets a person", |_args| {
            Box::pin(async move { Ok(json!({"greeting": "hello"})) })
        })
        .param("name", "string", "The person's name", true)
        .param("formal", "boolean", "Use formal greeting", false)
        .build();

        let callbacks: Vec<Arc<dyn Callback>> = vec![Arc::new(callback)];
        let tools = export_tool_schemas(&callbacks, ToolFormat::OpenAI);

        let tools_arr = tools.as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);

        let tool = &tools_arr[0];
        assert_eq!(tool["function"]["name"], "greet");
        assert!(tool["function"]["parameters"]["properties"]["name"].is_object());
    }

    #[test]
    fn tool_call_error_display() {
        let err = ToolCallError::MissingField("name".to_string());
        assert!(err.to_string().contains("missing required field"));
        assert!(err.to_string().contains("name"));

        let err = ToolCallError::InvalidArguments("bad json".to_string());
        assert!(err.to_string().contains("invalid arguments"));

        let err = ToolCallError::ToolNotFound("unknown".to_string());
        assert!(err.to_string().contains("tool not found"));
    }

    #[test]
    fn tool_call_error_from_callback_error() {
        let cb_err = CallbackError::ExecutionFailed("test".to_string());
        let tool_err: ToolCallError = cb_err.into();
        assert!(matches!(tool_err, ToolCallError::ExecutionFailed(_)));
    }

    #[test]
    fn parsed_tool_call_is_debug() {
        let parsed = ParsedToolCall {
            id: Some("test_id".to_string()),
            name: "echo".to_string(),
            arguments: json!({"message": "hello"}),
        };

        let debug = format!("{:?}", parsed);
        assert!(debug.contains("ParsedToolCall"));
        assert!(debug.contains("echo"));
    }

    #[test]
    fn parsed_tool_call_is_clone() {
        let parsed = ParsedToolCall {
            id: Some("test_id".to_string()),
            name: "echo".to_string(),
            arguments: json!({"message": "hello"}),
        };

        let cloned = parsed.clone();
        assert_eq!(cloned.id, Some("test_id".to_string()));
        assert_eq!(cloned.name, "echo");
    }
}
