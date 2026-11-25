//! In-process MCP servers (`create_sdk_mcp_server`) and the `tool` helper.
//!
//! An SDK MCP server runs inside your process instead of as a subprocess. The
//! CLI speaks JSON-RPC to it over the control protocol; this module answers
//! `initialize`, `tools/list`, `tools/call` and `ping` directly, dispatching
//! `tools/call` to the registered [`ToolHandler`]s. This mirrors the Python
//! SDK's `create_sdk_mcp_server` / `SdkMcpBridge`, without pulling in a full
//! MCP implementation (the served surface is small and fully specified).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::errors::Result;
use crate::types::ToolAnnotations;

/// Protocol version this server reports from `initialize`.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// The result a tool handler returns.
///
/// Build one with [`ToolResponse::text`] for the common case, or construct it
/// directly for images / multiple blocks. Set `is_error` to report a failure
/// to the model.
#[derive(Debug, Clone, Default)]
pub struct ToolResponse {
    /// MCP content blocks (`{"type": "text", "text": ...}`, `{"type":
    /// "image", ...}`, ...).
    pub content: Vec<Value>,
    /// Whether this result represents an error.
    pub is_error: bool,
}

impl ToolResponse {
    /// A single text block.
    pub fn text(text: impl Into<String>) -> Self {
        ToolResponse {
            content: vec![json!({"type": "text", "text": text.into()})],
            is_error: false,
        }
    }

    /// A single text block flagged as an error.
    pub fn error(text: impl Into<String>) -> Self {
        ToolResponse {
            content: vec![json!({"type": "text", "text": text.into()})],
            is_error: true,
        }
    }

    /// Arbitrary content blocks.
    pub fn blocks(content: Vec<Value>) -> Self {
        ToolResponse {
            content,
            is_error: false,
        }
    }

    /// Mark the response as an error.
    pub fn with_is_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    fn to_call_result(&self) -> Value {
        json!({ "content": self.content, "isError": self.is_error })
    }
}

type ToolHandlerFn =
    dyn Fn(Value) -> crate::types::BoxFuture<'static, Result<ToolResponse>> + Send + Sync;

/// A single in-process tool: its name, description, input schema and handler.
///
/// Create one with [`tool`].
#[derive(Clone)]
pub struct SdkMcpTool {
    name: String,
    description: String,
    input_schema: Value,
    annotations: Option<ToolAnnotations>,
    handler: Arc<ToolHandlerFn>,
    /// Compiled once at construction. Compiling a JSON Schema is far more
    /// expensive than validating against it, so doing it per `tools/call` (as
    /// the first cut did) made every tool invocation pay a schema compile.
    #[cfg(feature = "validate")]
    validator: Option<Arc<jsonschema::Validator>>,
}

impl SdkMcpTool {
    /// The tool's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn wire_definition(&self) -> Value {
        let mut def = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(annotations) = &self.annotations {
            if let Ok(value) = serde_json::to_value(annotations) {
                if value.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                    def["annotations"] = value;
                }
            }
            if let Some(max_size) = annotations.max_result_size_chars {
                def["_meta"] = json!({ "anthropic/maxResultSizeChars": max_size });
            }
        }
        def
    }
}

impl std::fmt::Debug for SdkMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkMcpTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// Define an in-process MCP tool.
///
/// `input_schema` is a JSON Schema object (use [`serde_json::json!`]); the
/// handler is an async function taking the parsed arguments and returning a
/// [`ToolResponse`]. With the `validate` feature (on by default) arguments are
/// checked against the schema before the handler runs, and an invalid input is
/// reported to the model as an error result.
///
/// ```
/// use claude_agent_sdk::{tool, ToolResponse};
/// use serde_json::json;
///
/// let add = tool(
///     "add",
///     "Add two numbers",
///     json!({
///         "type": "object",
///         "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
///         "required": ["a", "b"],
///     }),
///     |args| async move {
///         let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
///         Ok(ToolResponse::text(format!("Sum: {sum}")))
///     },
/// );
/// ```
pub fn tool<F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    input_schema: Value,
    handler: F,
) -> SdkMcpTool
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolResponse>> + Send + 'static,
{
    #[cfg(feature = "validate")]
    let validator = jsonschema::validator_for(&input_schema).ok().map(Arc::new);
    SdkMcpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
        annotations: None,
        handler: Arc::new(move |args| Box::pin(handler(args))),
        #[cfg(feature = "validate")]
        validator,
    }
}

/// Like [`tool`], but also attaches [`ToolAnnotations`].
pub fn tool_with_annotations<F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    input_schema: Value,
    annotations: ToolAnnotations,
    handler: F,
) -> SdkMcpTool
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolResponse>> + Send + 'static,
{
    SdkMcpTool {
        annotations: Some(annotations),
        ..tool(name, description, input_schema, handler)
    }
}

/// An in-process MCP server: a named collection of [`SdkMcpTool`]s.
///
/// Returned by [`create_sdk_mcp_server`] wrapped in an [`Arc`]; put it in an
/// [`McpServerConfig::Sdk`](crate::McpServerConfig::Sdk) entry of
/// [`ClaudeAgentOptions::mcp_servers`](crate::ClaudeAgentOptions::mcp_servers).
pub struct SdkMcpServer {
    name: String,
    version: String,
    tools: HashMap<String, SdkMcpTool>,
    tool_order: Vec<String>,
    /// The `tools/list` result, built once; the tool set is immutable.
    tool_list: Value,
}

impl SdkMcpServer {
    /// The server's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn build_tool_list(tools: &HashMap<String, SdkMcpTool>, order: &[String]) -> Value {
        let defs: Vec<Value> = order
            .iter()
            .filter_map(|name| tools.get(name))
            .map(SdkMcpTool::wire_definition)
            .collect();
        json!({ "tools": defs })
    }

    /// Handle one JSON-RPC message (as the CLI sends over the control channel).
    ///
    /// Returns `Some(response)` for a request and `None` for a notification
    /// (nothing to send back). Protocol-level problems come back as JSON-RPC
    /// error objects; a tool that fails is reported as an error *result*,
    /// matching the Python bridge. Public so a server can be exercised directly
    /// in tests or served over a custom transport.
    pub async fn handle_jsonrpc(&self, message: &Value) -> Option<Value> {
        // A response or a message with no method: nothing to do.
        let method = message.get("method").and_then(Value::as_str)?;
        // Notifications (no id) get no reply.
        let id = message.get("id").cloned()?;

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": self.name, "version": self.version },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tool_list.clone()),
            "tools/call" => self.call_tool(message.get("params")).await,
            other => Err(JsonRpcError::method_not_found(other)),
        };

        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": err.to_value() }),
        })
    }

    async fn call_tool(&self, params: Option<&Value>) -> std::result::Result<Value, JsonRpcError> {
        let params = params.ok_or_else(|| JsonRpcError::invalid_params("missing params"))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("missing tool name"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let Some(tool) = self.tools.get(name) else {
            // A missing tool is reported as an error *result*, like Python.
            return Ok(ToolResponse::error(format!("Tool '{name}' not found")).to_call_result());
        };

        #[cfg(feature = "validate")]
        if let Some(validator) = &tool.validator {
            if let Err(error) = validator.validate(&arguments) {
                return Ok(
                    ToolResponse::error(format!("Input validation error: {error}"))
                        .to_call_result(),
                );
            }
        }

        let response = match (tool.handler)(arguments).await {
            Ok(response) => response,
            Err(err) => ToolResponse::error(err.to_string()),
        };
        Ok(response.to_call_result())
    }
}

impl std::fmt::Debug for SdkMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkMcpServer")
            .field("name", &self.name)
            .field("tools", &self.tool_order)
            .finish()
    }
}

/// Create an in-process MCP server.
///
/// ```
/// use claude_agent_sdk::{create_sdk_mcp_server, tool, ToolResponse};
/// use serde_json::json;
///
/// let greet = tool(
///     "greet",
///     "Greet a user",
///     json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
///     |args| async move {
///         Ok(ToolResponse::text(format!("Hello, {}!", args["name"].as_str().unwrap_or("friend"))))
///     },
/// );
/// let server = create_sdk_mcp_server("greeter", "1.0.0", vec![greet]);
/// ```
pub fn create_sdk_mcp_server(
    name: impl Into<String>,
    version: impl Into<String>,
    tools: Vec<SdkMcpTool>,
) -> Arc<SdkMcpServer> {
    let mut map = HashMap::new();
    let mut order = Vec::with_capacity(tools.len());
    for tool in tools {
        if !map.contains_key(&tool.name) {
            order.push(tool.name.clone());
        }
        map.insert(tool.name.clone(), tool);
    }
    let tool_list = SdkMcpServer::build_tool_list(&map, &order);
    Arc::new(SdkMcpServer {
        name: name.into(),
        version: version.into(),
        tools: map,
        tool_order: order,
        tool_list,
    })
}

struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    fn method_not_found(method: &str) -> Self {
        JsonRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
        }
    }

    fn invalid_params(message: &str) -> Self {
        JsonRpcError {
            code: -32602,
            message: message.to_string(),
        }
    }

    fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc_server() -> Arc<SdkMcpServer> {
        let add = tool(
            "add",
            "Add two numbers",
            json!({
                "type": "object",
                "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                "required": ["a", "b"],
            }),
            |args| async move {
                let sum = args["a"].as_f64().unwrap() + args["b"].as_f64().unwrap();
                Ok(ToolResponse::text(format!("Sum: {sum}")))
            },
        );
        create_sdk_mcp_server("calc", "1.0.0", vec![add])
    }

    #[tokio::test]
    async fn initialize_and_list() {
        let server = calc_server();
        let init = server
            .handle_jsonrpc(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .await
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "calc");
        assert_eq!(init["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);

        let list = server
            .handle_jsonrpc(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "add");
    }

    #[tokio::test]
    async fn call_tool_runs_handler() {
        let server = calc_server();
        let response = server
            .handle_jsonrpc(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "add", "arguments": {"a": 2, "b": 3}},
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["content"][0]["text"], "Sum: 5");
        assert_eq!(response["result"]["isError"], false);
    }

    #[tokio::test]
    async fn unknown_tool_is_error_result() {
        let server = calc_server();
        let response = server
            .handle_jsonrpc(&json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "nope", "arguments": {}},
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    #[cfg(feature = "validate")]
    #[tokio::test]
    async fn invalid_arguments_rejected() {
        let server = calc_server();
        let response = server
            .handle_jsonrpc(&json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {"name": "add", "arguments": {"a": 2}},
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("validation"));
    }

    #[tokio::test]
    async fn notification_gets_no_reply() {
        let server = calc_server();
        let reply = server
            .handle_jsonrpc(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert!(reply.is_none());
    }

    #[tokio::test]
    async fn ping_answered() {
        let server = calc_server();
        let reply = server
            .handle_jsonrpc(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
            .await
            .unwrap();
        assert_eq!(reply["result"], json!({}));
    }
}
