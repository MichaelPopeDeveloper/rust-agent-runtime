//! Ask for structured JSON output validated against a schema. The final value
//! arrives on `ResultMessage::structured_output`.
//!
//! Run with: `cargo run --example structured_output`

use claude_agent_sdk::{query, ClaudeAgentOptions, Message, OutputFormat};
use futures::StreamExt;
use serde_json::json;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let schema = json!({
        "type": "object",
        "properties": {
            "language": {"type": "string"},
            "year_created": {"type": "integer"},
        },
        "required": ["language", "year_created"],
        "additionalProperties": false,
    });

    let options = ClaudeAgentOptions::builder()
        .output_format(OutputFormat::json_schema(schema))
        .build();

    let mut stream = query(
        "In what year was the Rust language first released? Name the language too.",
        options,
    )
    .await?;
    while let Some(message) = stream.next().await {
        if let Message::Result(result) = message? {
            match result.structured_output {
                Some(value) => println!("Structured output: {value:#}"),
                None => println!("No structured output (subtype: {})", result.subtype),
            }
        }
    }
    Ok(())
}
