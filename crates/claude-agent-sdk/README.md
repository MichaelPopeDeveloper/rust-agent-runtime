# Claude Agent SDK for Rust

A Rust port of Anthropic's [Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-python)
(which ships for Python and TypeScript). It drives the `claude` Claude Code CLI
over its stream-JSON control protocol, so a Rust program can run the full Claude
Code agent loop — built-in tools, MCP servers, subagents, hooks, permissions —
and receive typed messages back.

It speaks the same wire protocol as the official SDKs and talks to the same
`claude` binary. It is a community port, not an official Anthropic product.

## Requirements

- The `claude` CLI, installed and authenticated:
  ```bash
  npm install -g @anthropic-ai/claude-code
  ```
  Or point [`ClaudeAgentOptions::cli_path`] at a `claude` executable.
- A [Tokio](https://tokio.rs) runtime.

## Install

```toml
[dependencies]
claude-agent-sdk = "0.1"
tokio = { version = "1", features = ["full"] }
futures = "0.3"
```

## Quick start

```rust,no_run
use claude_agent_sdk::{query, ClaudeAgentOptions, Message, ContentBlock};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions::builder()
        .model("claude-opus-5-5")
        .build();

    let mut stream = query("What is the capital of France?", options).await?;
    while let Some(message) = stream.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in &assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("{}", text.text);
                }
            }
        }
    }
    Ok(())
}
```

## Two entry points

| Use case | API |
|---|---|
| One prompt, read the answer and stop | [`query`] |
| Interactive, multi-turn: follow-ups, interrupts, mode/model changes | [`ClaudeSDKClient`] |

### Interactive sessions

```rust,no_run
use claude_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message, ContentBlock};
use futures::StreamExt;

# async fn run() -> claude_agent_sdk::Result<()> {
let mut client = ClaudeSDKClient::new(ClaudeAgentOptions::default());
client.connect().await?;

client.query("Draft a commit message for my staged changes.", "default").await?;
let mut responses = client.receive_response();
while let Some(message) = responses.next().await {
    if let Message::Assistant(a) = message? {
        for block in &a.content {
            if let ContentBlock::Text(t) = block { println!("{}", t.text); }
        }
    }
}
drop(responses);

client.disconnect().await;
# Ok(())
# }
```

## In-process tools (SDK MCP servers)

Expose Rust functions to Claude as tools that run inside your process — no
subprocess, no IPC:

```rust,no_run
use claude_agent_sdk::{create_sdk_mcp_server, tool, ToolResponse, McpServerConfig, McpServers, ClaudeAgentOptions};
use serde_json::json;
use std::collections::HashMap;

let add = tool(
    "add",
    "Add two numbers",
    json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}),
    |args| async move {
        let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
        Ok(ToolResponse::text(format!("{sum}")))
    },
);
let server = create_sdk_mcp_server("calc", "1.0.0", vec![add]);

let mut servers = HashMap::new();
servers.insert("calc".to_string(), McpServerConfig::Sdk(server));

let options = ClaudeAgentOptions::builder()
    .mcp_servers(McpServers::Map(servers))
    .allowed_tools(["mcp__calc__add"])   // SDK tools are named mcp__<server>__<tool>
    .build();
```

## Permissions and hooks

- A `can_use_tool` callback decides tool calls in code (allow, allow with a
  modified input, or deny).
- Typed `hooks` (`PreToolUse`, `PostToolUse`, `Stop`, ...) observe or gate the
  agent loop and run over the same control protocol.

See [`examples/tool_permission_callback.rs`](examples/tool_permission_callback.rs)
and [`examples/hooks.rs`](examples/hooks.rs).

## Examples

Each is runnable with `cargo run --example <name>` (they need the `claude` CLI):

| Example | Shows |
|---|---|
| `quick_start` | A basic one-shot query |
| `streaming_client` | Multi-turn `ClaudeSDKClient` |
| `mcp_calculator` | In-process MCP tools |
| `hooks` | A `PreToolUse` hook gating Bash |
| `tool_permission_callback` | A `can_use_tool` permission handler |
| `system_prompt` | Appending to Claude Code's built-in prompt |
| `agents` | Defining a subagent programmatically |
| `structured_output` | JSON output validated against a schema |
| `stderr_callback` | Capturing CLI stderr |

## Cargo features

- `validate` (default): validate SDK MCP tool arguments against each tool's JSON
  Schema before the handler runs. Disable it to drop the `jsonschema`
  dependency.

## How it works

The SDK spawns `claude --output-format stream-json --input-format stream-json`
and speaks its newline-delimited JSON protocol: an `initialize` handshake
carrying hooks, agents and skills; user messages on stdin; assistant, system,
and result frames on stdout. When the CLI needs the host — a permission
decision, a hook, or an in-process MCP call — it sends a `control_request` and
the SDK answers with a `control_response`. This mirrors the Python and
TypeScript SDKs, so behavior and options line up with their documentation.

## Relationship to the official SDKs

This is an independent Rust implementation of the same client protocol. It is
not affiliated with or endorsed by Anthropic. "Claude" and "Claude Code" are
Anthropic's.

## License

MIT. See [LICENSE](LICENSE).

[`query`]: https://docs.rs/claude-agent-sdk/latest/claude_agent_sdk/fn.query.html
[`ClaudeSDKClient`]: https://docs.rs/claude-agent-sdk/latest/claude_agent_sdk/struct.ClaudeSDKClient.html
[`ClaudeAgentOptions::cli_path`]: https://docs.rs/claude-agent-sdk/latest/claude_agent_sdk/struct.ClaudeAgentOptions.html
