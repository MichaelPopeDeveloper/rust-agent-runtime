#![allow(missing_docs)] // self-describing DTO fields; types are documented above each definition

//! MCP server configuration and the in-process SDK server handle.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::SdkMcpServer;

/// Configuration for one MCP (Model Context Protocol) server.
///
/// External servers (`stdio`, `sse`, `http`) are passed to the CLI via
/// `--mcp-config`. An [`McpServerConfig::Sdk`] server runs in-process and is
/// served to the CLI over the control protocol instead.
#[derive(Clone)]
pub enum McpServerConfig {
    /// A server launched as a subprocess that speaks MCP over stdio.
    Stdio {
        /// Executable to run.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Extra environment variables.
        env: HashMap<String, String>,
    },
    /// A server reached over Server-Sent Events.
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
    /// A server reached over streamable HTTP.
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    /// An in-process server created with
    /// [`create_sdk_mcp_server`](crate::create_sdk_mcp_server).
    Sdk(Arc<SdkMcpServer>),
}

impl McpServerConfig {
    /// A stdio server running `command` with no args or extra env.
    pub fn stdio(command: impl Into<String>) -> Self {
        McpServerConfig::Stdio {
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    /// A stdio server with arguments.
    pub fn stdio_with_args<I, S>(command: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        McpServerConfig::Stdio {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: HashMap::new(),
        }
    }

    /// An SSE server at `url`.
    pub fn sse(url: impl Into<String>) -> Self {
        McpServerConfig::Sse {
            url: url.into(),
            headers: HashMap::new(),
        }
    }

    /// An HTTP server at `url`.
    pub fn http(url: impl Into<String>) -> Self {
        McpServerConfig::Http {
            url: url.into(),
            headers: HashMap::new(),
        }
    }

    /// Whether this is an in-process SDK server.
    pub fn is_sdk(&self) -> bool {
        matches!(self, McpServerConfig::Sdk(_))
    }

    /// The in-process server instance, if this is an SDK server.
    pub(crate) fn sdk_instance(&self) -> Option<Arc<SdkMcpServer>> {
        match self {
            McpServerConfig::Sdk(server) => Some(server.clone()),
            _ => None,
        }
    }

    /// The JSON the CLI expects for this server under `--mcp-config`.
    ///
    /// For an SDK server this is just `{"type": "sdk", "name": ...}` (the CLI
    /// is told the server exists; its traffic flows over the control channel).
    pub(crate) fn to_cli_json(&self) -> Value {
        match self {
            McpServerConfig::Stdio { command, args, env } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("stdio".into()));
                obj.insert("command".into(), Value::String(command.clone()));
                if !args.is_empty() {
                    obj.insert(
                        "args".into(),
                        serde_json::to_value(args).unwrap_or_default(),
                    );
                }
                if !env.is_empty() {
                    obj.insert("env".into(), serde_json::to_value(env).unwrap_or_default());
                }
                Value::Object(obj)
            }
            McpServerConfig::Sse { url, headers } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("sse".into()));
                obj.insert("url".into(), Value::String(url.clone()));
                if !headers.is_empty() {
                    obj.insert(
                        "headers".into(),
                        serde_json::to_value(headers).unwrap_or_default(),
                    );
                }
                Value::Object(obj)
            }
            McpServerConfig::Http { url, headers } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("http".into()));
                obj.insert("url".into(), Value::String(url.clone()));
                if !headers.is_empty() {
                    obj.insert(
                        "headers".into(),
                        serde_json::to_value(headers).unwrap_or_default(),
                    );
                }
                Value::Object(obj)
            }
            McpServerConfig::Sdk(server) => {
                serde_json::json!({ "type": "sdk", "name": server.name() })
            }
        }
    }
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpServerConfig::Stdio { command, args, .. } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .finish(),
            McpServerConfig::Sse { url, .. } => f.debug_struct("Sse").field("url", url).finish(),
            McpServerConfig::Http { url, .. } => f.debug_struct("Http").field("url", url).finish(),
            McpServerConfig::Sdk(server) => {
                f.debug_struct("Sdk").field("name", &server.name()).finish()
            }
        }
    }
}

/// Where MCP servers are configured: a map of named servers, or a path/inline
/// JSON passed straight to `--mcp-config`.
#[derive(Debug, Clone)]
pub enum McpServers {
    /// Named server configurations.
    Map(HashMap<String, McpServerConfig>),
    /// A path to an MCP config JSON file.
    Path(std::path::PathBuf),
    /// An inline JSON string.
    Json(String),
}

impl McpServers {
    /// The SDK (in-process) servers, keyed by name.
    pub(crate) fn sdk_servers(&self) -> HashMap<String, Arc<SdkMcpServer>> {
        let mut servers = HashMap::new();
        if let McpServers::Map(map) = self {
            for (name, config) in map {
                if let Some(instance) = config.sdk_instance() {
                    servers.insert(name.clone(), instance);
                }
            }
        }
        servers
    }

    /// Whether there are no servers configured.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            McpServers::Map(map) => map.is_empty(),
            McpServers::Path(_) | McpServers::Json(_) => false,
        }
    }
}

impl From<HashMap<String, McpServerConfig>> for McpServers {
    fn from(map: HashMap<String, McpServerConfig>) -> Self {
        McpServers::Map(map)
    }
}

/// MCP tool annotation hints advertised to Claude (wire format is camelCase).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(
        rename = "readOnlyHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only_hint: Option<bool>,
    #[serde(
        rename = "destructiveHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub destructive_hint: Option<bool>,
    #[serde(
        rename = "idempotentHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotent_hint: Option<bool>,
    #[serde(
        rename = "openWorldHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_world_hint: Option<bool>,
    /// Raises the size up to which Claude Code keeps this tool's result inline
    /// instead of persisting it to a file. Sent as
    /// `_meta["anthropic/maxResultSizeChars"]`.
    #[serde(skip)]
    pub max_result_size_chars: Option<u64>,
}

// ---------------------------------------------------------------------------
// MCP status (returned by get_mcp_status)
// ---------------------------------------------------------------------------

/// Connection status of an MCP server, from `get_mcp_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// Server name as configured.
    pub name: String,
    /// Connection status (`connected`, `pending`, `failed`, `needs-auth`,
    /// `disabled`).
    pub status: String,
    /// Server info from the MCP handshake (when connected).
    #[serde(
        rename = "serverInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_info: Option<Value>,
    /// Error message (when `status` is `failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tools the server provides (when connected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    /// Any other fields.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Response from `get_mcp_status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpStatusResponse {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Vec<McpServerStatus>,
}
