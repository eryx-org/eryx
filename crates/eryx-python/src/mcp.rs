//! MCP (Model Context Protocol) integration for Eryx.
//!
//! This module provides the `MCPManager` class that connects to MCP servers
//! and exposes their tools as native Rust callbacks. When sandboxed code calls
//! `await mcp.server.tool(args)`, the entire path is Rust — no Python GIL:
//!
//! ```text
//! Python sandbox → WASM invoke → callback_handler → DynamicCallback → rmcp → MCP server
//! ```

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use eryx::{CallbackError, DynamicCallback, Schema};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::error::InitializationError;

/// A single MCP server connection with its tools.
struct MCPConnection {
    name: String,
    service: RunningService<RoleClient, ()>,
    tools: Vec<Tool>,
}

impl std::fmt::Debug for MCPConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MCPConnection")
            .field("name", &self.name)
            .field("tools", &self.tools.len())
            .finish()
    }
}

/// Manager for MCP server connections.
///
/// Creates connections to MCP servers and exposes their tools as native Rust
/// callbacks that can be used by `Sandbox` and `Session`.
///
/// Example:
///     from eryx._eryx import MCPManager
///
///     manager = MCPManager()
///     manager.connect("filesystem", "npx", ["-y", "@anthropic/mcp-server-filesystem", "."], {})
///     tools = manager.list_tools()
///     print(tools)  # [{"name": "mcp[\"filesystem\"].read_file", ...}, ...]
///
///     sandbox = Sandbox(mcp=manager)
///     result = sandbox.execute('data = await mcp["filesystem"].read_file(path="README.md")')
#[pyclass(module = "eryx")]
pub struct MCPManager {
    /// Tokio runtime owned by this manager. Kept alive via Arc so that
    /// DynamicCallback closures can use it even after MCPManager is dropped
    /// from Python (the Arc prevents the runtime from shutting down).
    runtime: Arc<tokio::runtime::Runtime>,
    /// Active MCP server connections.
    connections: Vec<MCPConnection>,
}

// SAFETY: RunningService is Send + Sync (it communicates over channels).
// We only access connections from methods that hold &self or &mut self,
// and the GIL serializes Python-side access.
unsafe impl Send for MCPManager {}
unsafe impl Sync for MCPManager {}

#[pymethods]
impl MCPManager {
    /// Create a new empty MCP manager.
    #[new]
    fn new() -> PyResult<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );
        Ok(Self {
            runtime,
            connections: Vec::new(),
        })
    }

    /// Connect to an MCP server by spawning a child process.
    ///
    /// Args:
    ///     name: A human-readable name for this server connection.
    ///     command: The command to spawn (e.g., "npx", "uvx").
    ///     args: Arguments to pass to the command.
    ///     env: Environment variables to set for the child process.
    ///     timeout_secs: Timeout in seconds for the connection handshake.
    ///
    /// Returns:
    ///     The number of tools available from this server.
    ///
    /// Raises:
    ///     InitializationError: If the connection fails.
    #[pyo3(signature = (name, command, args=vec![], env=HashMap::new(), timeout_secs=30.0))]
    fn connect(
        &mut self,
        py: Python<'_>,
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        timeout_secs: f64,
    ) -> PyResult<usize> {
        let runtime = self.runtime.clone();
        let timeout = std::time::Duration::from_secs_f64(timeout_secs);

        py.detach(|| {
            runtime.block_on(async {
                // Build the child process command
                let mut cmd = tokio::process::Command::new(&command);
                cmd.args(&args);
                for (k, v) in &env {
                    cmd.env(k, v);
                }

                let transport = TokioChildProcess::new(cmd).map_err(|e| {
                    InitializationError::new_err(format!(
                        "failed to spawn MCP server '{name}' ({command}): {e}"
                    ))
                })?;

                // Connect and perform the MCP handshake
                let service = tokio::time::timeout(timeout, ().serve(transport))
                    .await
                    .map_err(|_| {
                        InitializationError::new_err(format!(
                            "MCP server '{name}' connection timed out after {timeout_secs}s"
                        ))
                    })?
                    .map_err(|e| {
                        InitializationError::new_err(format!(
                            "MCP handshake failed for '{name}': {e}"
                        ))
                    })?;

                // List available tools
                let tools_resp = service.list_tools(Default::default()).await.map_err(|e| {
                    InitializationError::new_err(format!("failed to list tools from '{name}': {e}"))
                })?;

                let tool_count = tools_resp.tools.len();

                self.connections.push(MCPConnection {
                    name,
                    service,
                    tools: tools_resp.tools,
                });

                Ok(tool_count)
            })
        })
    }

    /// Get the names of all connected MCP servers.
    #[getter]
    fn server_names(&self) -> Vec<String> {
        self.connections.iter().map(|c| c.name.clone()).collect()
    }

    /// List all available tools across all connected servers.
    ///
    /// Returns:
    ///     A list of dicts, each with "name", "description", and "schema" keys.
    fn list_tools<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let mut result = Vec::new();
        for conn in &self.connections {
            for tool in &conn.tools {
                let dict = PyDict::new(py);
                dict.set_item("name", format!(r#"mcp["{}"].{}"#, conn.name, tool.name))?;
                dict.set_item("description", tool.description.as_deref().unwrap_or(""))?;
                let schema_value = serde_json::to_value(&*tool.input_schema)
                    .unwrap_or(Value::Object(Default::default()));
                let py_schema = pythonize::pythonize(py, &schema_value).map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "failed to convert schema: {e}"
                    ))
                })?;
                dict.set_item("schema", py_schema)?;
                result.push(dict);
            }
        }
        Ok(result)
    }

    /// Gracefully shut down all MCP server connections.
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        let runtime = self.runtime.clone();
        let connections = std::mem::take(&mut self.connections);

        py.detach(|| {
            runtime.block_on(async {
                for conn in connections {
                    // cancel() gracefully shuts down the connection
                    let _ = conn.service.cancel().await;
                }
            });
            Ok(())
        })
    }

    /// Call a tool on a connected MCP server.
    ///
    /// The tool name can use either dot notation (``server.tool``) or bracket
    /// notation (``mcp["server"].tool``).
    ///
    /// Args:
    ///     name: The qualified tool name (e.g. ``"fs.read_file"``).
    ///     arguments: Optional dict of arguments to pass to the tool.
    ///
    /// Returns:
    ///     The tool result as a Python object (parsed JSON).
    ///
    /// Raises:
    ///     ValueError: If the tool name format is invalid or the server/tool is not found.
    ///     RuntimeError: If the tool call fails.
    #[pyo3(signature = (name, arguments=None))]
    fn call_tool(
        &self,
        py: Python<'_>,
        name: String,
        arguments: Option<Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let (server_name, tool_name) =
            parse_tool_name(&name).map_err(|e| pyo3::exceptions::PyValueError::new_err(e))?;

        let conn = self
            .connections
            .iter()
            .find(|c| c.name == server_name)
            .ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown MCP server '{server_name}'. Connected servers: {}",
                    self.connections
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        // Verify the tool exists on this server
        if !conn.tools.iter().any(|t| t.name.as_ref() == tool_name) {
            let available: Vec<&str> = conn.tools.iter().map(|t| t.name.as_ref()).collect();
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown tool '{tool_name}' on server '{server_name}'. Available: {}",
                available.join(", ")
            )));
        }

        // Convert Python dict to serde_json Map
        let json_args: Option<serde_json::Map<String, Value>> = if let Some(dict) = arguments {
            let val: Value = pythonize::depythonize(&dict).map_err(|e| {
                pyo3::exceptions::PyValueError::new_err(format!("failed to convert arguments: {e}"))
            })?;
            match val {
                Value::Object(map) => Some(map),
                _ => {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "arguments must be a dict",
                    ));
                }
            }
        } else {
            None
        };

        let peer = conn.service.peer().clone();
        let runtime = self.runtime.clone();
        let tool_name_cow: Cow<'static, str> = tool_name.to_string().into();

        py.detach(|| {
            let result_value = runtime.block_on(async {
                let result = peer
                    .call_tool(CallToolRequestParams {
                        meta: None,
                        name: tool_name_cow,
                        arguments: json_args,
                        task: None,
                    })
                    .await
                    .map_err(|e| {
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "MCP call_tool failed: {e}"
                        ))
                    })?;

                mcp_result_to_value(result)
            })?;

            Python::attach(|py| {
                pythonize::pythonize(py, &result_value)
                    .map(|obj| obj.unbind())
                    .map_err(|e| {
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "failed to convert result: {e}"
                        ))
                    })
            })
        })
    }

    fn __repr__(&self) -> String {
        let servers: Vec<&str> = self.connections.iter().map(|c| c.name.as_str()).collect();
        let tool_count: usize = self.connections.iter().map(|c| c.tools.len()).sum();
        format!(
            "MCPManager(servers=[{}], tools={})",
            servers.join(", "),
            tool_count
        )
    }
}

/// Parse a qualified tool name into (server, tool) components.
///
/// Accepts:
///   - ``server.tool`` — dot notation
///   - ``mcp["server"].tool`` — bracket notation (as returned by `list_tools`)
fn parse_tool_name(name: &str) -> Result<(&str, &str), String> {
    // Bracket notation: mcp["server"].tool
    if let Some(rest) = name.strip_prefix("mcp[\"") {
        if let Some((server, rest)) = rest.split_once("\"].")
            && !rest.is_empty()
        {
            return Ok((server, rest));
        }
        return Err(format!(
            "invalid tool name '{name}': expected mcp[\"server\"].tool format"
        ));
    }

    // Dot notation: server.tool
    if let Some((server, tool)) = name.split_once('.')
        && !server.is_empty()
        && !tool.is_empty()
    {
        return Ok((server, tool));
    }

    Err(format!(
        "invalid tool name '{name}': expected server.tool or mcp[\"server\"].tool format"
    ))
}

/// Convert an MCP `CallToolResult` to a `serde_json::Value`.
fn mcp_result_to_value(result: rmcp::model::CallToolResult) -> PyResult<Value> {
    // Check for error
    if result.is_error == Some(true) {
        let error_text: String = result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "MCP tool error: {error_text}"
        )));
    }

    // Extract text content from the result
    let text_parts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
        .collect();

    // If there's structured content, prefer it
    if let Some(structured) = result.structured_content {
        return Ok(structured);
    }

    // Try to parse the first text content as JSON
    if text_parts.len() == 1
        && let Ok(parsed) = serde_json::from_str(text_parts[0])
    {
        return Ok(parsed);
    }

    // Return text content as a JSON object
    let combined = text_parts.join("\n");
    Ok(serde_json::json!({ "text": combined }))
}

impl MCPManager {
    /// Convert all MCP tools into `DynamicCallback` instances.
    ///
    /// Each tool becomes a callback named `mcp.{server}.{tool}` that can be
    /// invoked from sandboxed code via `await mcp.server.tool(args)`.
    pub(crate) fn as_callbacks(&self) -> Vec<DynamicCallback> {
        let mut callbacks = Vec::new();

        for conn in &self.connections {
            // Get a peer handle for making RPC calls. The peer communicates
            // over channels so it's runtime-agnostic.
            let peer = conn.service.peer().clone();

            for tool in &conn.tools {
                let callback_name = format!("mcp.{}.{}", conn.name, tool.name);
                let tool_name: Cow<'static, str> = tool.name.clone().into_owned().into();
                let description = format!(
                    "[MCP: {}] {}",
                    conn.name,
                    tool.description.as_deref().unwrap_or("")
                );

                // Convert MCP input_schema (JsonObject) to eryx Schema
                let schema_value = serde_json::to_value(&*tool.input_schema)
                    .unwrap_or(Value::Object(Default::default()));
                let schema =
                    Schema::try_from_value(schema_value).unwrap_or_else(|_| eryx::empty_schema());

                // Clone what the closure needs
                let peer = peer.clone();
                let tool_name_for_closure = tool_name.clone();

                let handler = move |args: Value| -> Pin<
                    Box<dyn Future<Output = Result<Value, CallbackError>> + Send>,
                > {
                    let peer = peer.clone();
                    let tool_name = tool_name_for_closure.clone();

                    Box::pin(async move {
                        let arguments = match args {
                            Value::Object(map) => Some(map),
                            Value::Null => None,
                            _ => {
                                return Err(CallbackError::InvalidArguments(
                                    "MCP tool arguments must be an object".to_string(),
                                ));
                            }
                        };

                        let result = peer
                            .call_tool(CallToolRequestParams {
                                meta: None,
                                name: tool_name,
                                arguments,
                                task: None,
                            })
                            .await
                            .map_err(|e| {
                                CallbackError::ExecutionFailed(format!("MCP call_tool failed: {e}"))
                            })?;

                        // Check for error
                        if result.is_error == Some(true) {
                            let error_text: String = result
                                .content
                                .iter()
                                .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n");
                            return Err(CallbackError::ExecutionFailed(format!(
                                "MCP tool error: {error_text}"
                            )));
                        }

                        // Extract text content from the result
                        let text_parts: Vec<&str> = result
                            .content
                            .iter()
                            .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
                            .collect();

                        // If there's structured content, prefer it
                        if let Some(structured) = result.structured_content {
                            return Ok(structured);
                        }

                        // Try to parse the first text content as JSON
                        if text_parts.len() == 1
                            && let Ok(parsed) = serde_json::from_str(text_parts[0])
                        {
                            return Ok(parsed);
                        }

                        // Return text content as a JSON object
                        let combined = text_parts.join("\n");
                        Ok(serde_json::json!({ "text": combined }))
                    })
                };

                let callback =
                    DynamicCallback::new(callback_name, description, schema, Arc::new(handler));
                callbacks.push(callback);
            }
        }

        callbacks
    }
}

impl std::fmt::Debug for MCPManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MCPManager")
            .field("connections", &self.connections.len())
            .finish()
    }
}
