//! Decide tool calls in code with a `can_use_tool` callback.
//!
//! The callback runs whenever the CLI would otherwise *prompt* — tools already
//! allowed by `allowed_tools`, the permission mode, or the CLI's own safe-command
//! list never reach it. Writing a file via the shell is not pre-approved, so the
//! CLI asks us; we allow shell commands confined to one scratch directory and
//! deny everything else.
//!
//! Run with: `cargo run --example tool_permission_callback`

use claude_agent_sdk::{
    query, CanUseTool, ClaudeAgentOptions, ContentBlock, Message, PermissionResult,
};
use futures::StreamExt;

const SCRATCH: &str = "/tmp/claude-sdk-demo";

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let gate = CanUseTool::new(|tool_name, input, _ctx| async move {
        println!("[permission] {tool_name} requested with {input}");
        let command = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        // Only shell commands that stay inside the scratch directory.
        let confined = tool_name == "Bash"
            && command.contains(SCRATCH)
            && !command.contains("..")
            && !command.contains("rm ");
        if confined {
            Ok(PermissionResult::allow())
        } else {
            Ok(PermissionResult::deny(format!(
                "{tool_name} is only allowed inside {SCRATCH} in this demo"
            )))
        }
    });

    let options = ClaudeAgentOptions::builder().can_use_tool(gate).build();

    let prompt = format!(
        "Using the shell, create the directory {SCRATCH} if needed and write the text \
         'hello from the sdk' into {SCRATCH}/hello.txt. Then tell me the file's contents."
    );
    let mut stream = query(prompt, options).await?;
    while let Some(message) = stream.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in &assistant.content {
                if let ContentBlock::Text(text) = block {
                    println!("Claude: {}", text.text);
                }
            }
        }
    }
    match std::fs::read_to_string(format!("{SCRATCH}/hello.txt")) {
        Ok(contents) => println!("[verify] {SCRATCH}/hello.txt = {:?}", contents.trim()),
        Err(e) => println!("[verify] file not written: {e}"),
    }
    Ok(())
}
