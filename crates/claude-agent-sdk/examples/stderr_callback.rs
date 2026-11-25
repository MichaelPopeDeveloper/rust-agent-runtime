//! Capture the CLI subprocess's stderr for logging or debugging via the
//! `stderr` callback.
//!
//! Run with: `cargo run --example stderr_callback`

use claude_agent_sdk::{query, ClaudeAgentOptions, ContentBlock, Message, StderrCallback};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions::builder()
        .stderr(StderrCallback::new(|line| {
            eprintln!("[claude stderr] {line}");
        }))
        .build();

    let mut stream = query("Say hello.", options).await?;
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
