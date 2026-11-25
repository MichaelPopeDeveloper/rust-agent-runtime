//! The simplest possible query: ask once, print the answer.
//!
//! Run with: `cargo run --example quick_start`
//! Requires the `claude` CLI installed and authenticated.

use claude_agent_sdk::{query, ClaudeAgentOptions, ContentBlock, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions::builder()
        .system_prompt("You are a concise assistant.")
        .build();

    let mut stream = query("What is the capital of France?", options).await?;

    while let Some(message) = stream.next().await {
        match message? {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::Text(text) = block {
                        println!("Claude: {}", text.text);
                    }
                }
            }
            Message::Result(result) => {
                if let Some(cost) = result.total_cost_usd {
                    eprintln!("[cost: ${cost:.4}]");
                }
            }
            _ => {}
        }
    }
    Ok(())
}
