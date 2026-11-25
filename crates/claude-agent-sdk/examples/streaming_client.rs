//! A multi-turn, interactive session with [`ClaudeSDKClient`]: send a prompt,
//! read the response, then send a follow-up on the same conversation.
//!
//! Run with: `cargo run --example streaming_client`

use claude_agent_sdk::{ClaudeAgentOptions, ContentBlock, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut client = claude_agent_sdk::ClaudeSDKClient::new(ClaudeAgentOptions::default());
    client.connect().await?;

    for prompt in ["Pick a number between 1 and 10.", "Now double it."] {
        println!("\n>>> {prompt}");
        client.query(prompt, "default").await?;
        let mut responses = client.receive_response();
        while let Some(message) = responses.next().await {
            if let Message::Assistant(assistant) = message? {
                for block in &assistant.content {
                    if let ContentBlock::Text(text) = block {
                        println!("Claude: {}", text.text);
                    }
                }
            }
        }
    }

    client.disconnect().await;
    Ok(())
}
