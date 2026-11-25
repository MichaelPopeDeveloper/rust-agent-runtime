//! Use Claude Code's built-in system prompt and append your own instructions,
//! rather than replacing the prompt entirely.
//!
//! Run with: `cargo run --example system_prompt`

use claude_agent_sdk::{query, ClaudeAgentOptions, ContentBlock, Message, SystemPrompt};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions::builder()
        .system_prompt(SystemPrompt::claude_code_with_append(
            "Always answer in British English and keep it to one sentence.",
        ))
        .build();

    let mut stream = query("Explain what a borrow checker does.", options).await?;
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
