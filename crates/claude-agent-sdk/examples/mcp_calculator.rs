//! Expose in-process tools to Claude with an SDK MCP server. The tools run
//! inside this program — no separate process — and Claude can call them.
//!
//! Run with: `cargo run --example mcp_calculator`

use std::collections::HashMap;

use claude_agent_sdk::{
    create_sdk_mcp_server, query, tool, ClaudeAgentOptions, ContentBlock, McpServerConfig,
    McpServers, Message, ToolResponse,
};
use futures::StreamExt;
use serde_json::json;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let add = tool(
        "add",
        "Add two numbers",
        json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        }),
        |args| async move {
            let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
            Ok(ToolResponse::text(format!("{sum}")))
        },
    );

    let multiply = tool(
        "multiply",
        "Multiply two numbers",
        json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        }),
        |args| async move {
            let product = args["a"].as_f64().unwrap_or(0.0) * args["b"].as_f64().unwrap_or(0.0);
            Ok(ToolResponse::text(format!("{product}")))
        },
    );

    let server = create_sdk_mcp_server("calculator", "1.0.0", vec![add, multiply]);

    let mut servers = HashMap::new();
    servers.insert("calc".to_string(), McpServerConfig::Sdk(server));

    let options = ClaudeAgentOptions::builder()
        .mcp_servers(McpServers::Map(servers))
        // Tools from an SDK server are named mcp__<server>__<tool>.
        .allowed_tools(["mcp__calc__add", "mcp__calc__multiply"])
        .build();

    let mut stream = query("What is 2 + 40, then multiplied by 3?", options).await?;
    while let Some(message) = stream.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in &assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("Claude: {}", text.text);
                }
            }
        }
    }
    Ok(())
}
