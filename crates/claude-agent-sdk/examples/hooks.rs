//! Gate tool calls with a `PreToolUse` hook. This hook blocks any Bash command
//! containing `rm -rf`, letting everything else through.
//!
//! Run with: `cargo run --example hooks`

use std::collections::HashMap;

use claude_agent_sdk::{
    query, ClaudeAgentOptions, ContentBlock, HookCallback, HookEvent, HookInput, HookJsonOutput,
    HookMatcher, Message,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let guard = HookCallback::new(|input, _tool_use_id, _ctx| async move {
        if let Some(HookInput::PreToolUse(pre)) = HookInput::parse(&input) {
            let command = pre
                .tool_input
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if command.contains("rm -rf") {
                return Ok(HookJsonOutput::block(
                    "Refusing to run a destructive command.",
                ));
            }
        }
        Ok(HookJsonOutput::default())
    });

    let mut hooks = HashMap::new();
    hooks.insert(
        HookEvent::PreToolUse,
        vec![HookMatcher::new(Some("Bash"), vec![guard])],
    );

    let options = ClaudeAgentOptions::builder()
        .allowed_tools(["Bash"])
        .hooks(hooks)
        .build();

    let mut stream = query("List the files in the current directory.", options).await?;
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
