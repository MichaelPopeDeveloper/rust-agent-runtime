//! Define a custom subagent programmatically. The main agent can delegate to
//! it through the `Agent` tool.
//!
//! Run with: `cargo run --example agents`

use std::collections::HashMap;

use claude_agent_sdk::{query, AgentDefinition, ClaudeAgentOptions, ContentBlock, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut agents = HashMap::new();
    agents.insert(
        "haiku-writer".to_string(),
        AgentDefinition {
            model: Some("haiku".into()),
            tools: Some(vec![]),
            ..AgentDefinition::new(
                "Writes a single haiku on a given topic.",
                "You are a poet. Respond with exactly one haiku (5-7-5), nothing else.",
            )
        },
    );

    let options = ClaudeAgentOptions::builder().agents(agents).build();

    let mut stream = query(
        "Use the haiku-writer agent to write a haiku about Rust.",
        options,
    )
    .await?;
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
