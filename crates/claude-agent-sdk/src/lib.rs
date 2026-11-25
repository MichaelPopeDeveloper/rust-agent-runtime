//! # Claude Agent SDK for Rust
//!
//! A Rust port of Anthropic's Claude Agent SDK (which exists for
//! [Python](https://github.com/anthropics/claude-agent-sdk-python) and
//! TypeScript). It drives the `claude` Claude Code CLI over its stream-JSON
//! control protocol, so a Rust program can run the full Claude Code agent
//! loop — built-in tools, MCP servers, subagents, hooks, permissions — and
//! receive typed messages back.
//!
//! It speaks the same wire protocol as the official SDKs and talks to the same
//! `claude` binary; it is not an Anthropic product.
//!
//! ## Two entry points
//!
//! - [`query`] — a one-shot, unidirectional stream of messages for a single
//!   prompt. Ideal for "ask once, read the answer".
//! - [`ClaudeSDKClient`] — a bidirectional, stateful session: send follow-ups,
//!   interrupt, change the model or permission mode mid-conversation.
//!
//! ```no_run
//! use claude_agent_sdk::{query, ClaudeAgentOptions, Message, ContentBlock};
//! use futures::StreamExt;
//!
//! # async fn run() -> claude_agent_sdk::Result<()> {
//! let options = ClaudeAgentOptions::builder().model("claude-opus-5-5").build();
//! let mut stream = query("What is 2 + 2?", options).await?;
//! while let Some(message) = stream.next().await {
//!     if let Message::Assistant(assistant) = message? {
//!         for block in &assistant.content {
//!             if let ContentBlock::Text(text) = block {
//!                 println!("{}", text.text);
//!             }
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Requirements
//!
//! The `claude` CLI must be installed (`npm install -g
//! @anthropic-ai/claude-code`) and authenticated, or its path given via
//! [`ClaudeAgentOptions::cli_path`]. Everything runs on a Tokio runtime.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;

mod client;
mod errors;
mod mcp;
mod message_parser;
mod query;
mod transport;
mod types;

pub use client::ClaudeSDKClient;
pub use errors::{Error, Result, ResultError};
pub use mcp::{
    create_sdk_mcp_server, tool, tool_with_annotations, SdkMcpServer, SdkMcpTool, ToolResponse,
};
pub use message_parser::parse_message;
pub use transport::{MessageStream as RawMessageStream, SubprocessTransport, Transport};
pub use types::*;

pub use query::{Query, Queued};

/// A prompt for [`query`]: either a single string or a stream of raw
/// user-message dicts (streaming input mode).
pub enum Prompt {
    /// A single text prompt.
    Text(String),
    /// A stream of user-message JSON values, each like
    /// `{"type": "user", "message": {"role": "user", "content": "..."}}`.
    Stream(Pin<Box<dyn Stream<Item = Value> + Send>>),
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Prompt::Text(text.to_string())
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Prompt::Text(text)
    }
}

impl From<&String> for Prompt {
    fn from(text: &String) -> Self {
        Prompt::Text(text.clone())
    }
}

/// How long to wait for the `initialize` handshake. Overridable with
/// `CLAUDE_CODE_STREAM_CLOSE_TIMEOUT` (milliseconds), floored at 60s, matching
/// the Python SDK.
pub(crate) fn initialize_timeout() -> Duration {
    let ms = std::env::var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60_000);
    Duration::from_millis(ms.max(60_000))
}

/// Run a one-shot query and stream the resulting [`Message`]s.
///
/// This spawns the `claude` CLI, sends `prompt`, and returns a stream that
/// yields every message until the run completes. The CLI subprocess is torn
/// down when the returned [`QueryStream`] is dropped.
///
/// For interactive, multi-turn sessions (follow-ups, interrupts, mode changes)
/// use [`ClaudeSDKClient`] instead.
pub async fn query(prompt: impl Into<Prompt>, options: ClaudeAgentOptions) -> Result<QueryStream> {
    // Route permission prompts over stdio when a callback is set, *before* the
    // transport turns the options into CLI flags.
    let options = options.configured_for_can_use_tool()?;
    let transport = SubprocessTransport::new(Arc::new(options.clone()));
    query_with_transport(prompt, options, transport).await
}

/// Like [`query`], but drives a caller-supplied [`Transport`] instead of
/// spawning the `claude` CLI.
///
/// The SDK calls the transport's [`connect`](Transport::connect), sends the
/// prompt over [`write`](Transport::write), and runs the control protocol over
/// it. CLI-flag options (model, cwd, tools, ...) are only meaningful to the
/// default [`SubprocessTransport`]; a custom transport is responsible for
/// honoring whatever of them it cares about. Mirrors the `transport=` argument
/// of the Python and TypeScript SDKs, and is the seam used to test the protocol
/// without a subprocess.
pub async fn query_with_transport<T: Transport + 'static>(
    prompt: impl Into<Prompt>,
    options: ClaudeAgentOptions,
    mut transport: T,
) -> Result<QueryStream> {
    let prompt = prompt.into();
    let options = Arc::new(options.configured_for_can_use_tool()?);

    transport.connect().await?;
    let stream = transport
        .take_message_stream()
        .ok_or_else(|| Error::cli_connection("transport produced no message stream"))?;
    let transport: Arc<dyn Transport> = Arc::new(transport);

    let mut engine = Query::new(transport.clone(), &options);
    engine.start(stream);
    engine.initialize(initialize_timeout()).await?;

    match prompt {
        Prompt::Text(text) => {
            engine.write_user_message(&text, "default").await?;
            engine.close_input_when_done();
        }
        Prompt::Stream(mut messages) => {
            // Stream each user message, then close the input side.
            let input = engine.input_handle();
            tokio::spawn(async move {
                use futures::StreamExt;
                let mut wrote = false;
                while let Some(message) = messages.next().await {
                    wrote = true;
                    if input.write(message).await.is_err() {
                        break;
                    }
                }
                input.finish(wrote).await;
            });
        }
    }

    let rx = engine
        .take_messages()
        .ok_or_else(|| Error::cli_connection("message stream already taken"))?;
    Ok(QueryStream {
        rx,
        _engine: EngineGuard::new(engine),
    })
}

/// Keeps a [`Query`] (and thus the subprocess and reader task) alive while the
/// stream is consumed, and tears it down on drop.
struct EngineGuard {
    engine: Option<Query>,
}

impl EngineGuard {
    fn new(engine: Query) -> Self {
        EngineGuard {
            engine: Some(engine),
        }
    }
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            // `tokio::spawn` panics outside a runtime (e.g. a stream dropped
            // after `block_on` returned). Spawn the graceful close when we can;
            // otherwise just drop: the child was spawned with `kill_on_drop`,
            // so releasing the transport still terminates it.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    engine.close().await;
                });
            }
        }
    }
}

/// A stream of [`Message`]s from [`query`].
///
/// Yields `Result<Message>`; an [`Err`] means the CLI failed or a frame could
/// not be parsed. The stream ends after the CLI closes its output (normally
/// just after a [`Message::Result`]).
pub struct QueryStream {
    rx: mpsc::UnboundedReceiver<Result<Queued>>,
    _engine: EngineGuard,
}

impl Stream for QueryStream {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(queued))) => Poll::Ready(Some(parse_message(queued.into_value()))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl QueryStream {
    /// The next message, without needing `futures::StreamExt` in scope.
    pub async fn next_message(&mut self) -> Option<Result<Message>> {
        futures::StreamExt::next(self).await
    }

    /// Drain the stream and return every message. Stops at the first error.
    pub async fn collect_messages(mut self) -> Result<Vec<Message>> {
        let mut out = Vec::new();
        while let Some(message) = self.next_message().await {
            out.push(message?);
        }
        Ok(out)
    }

    /// Drain the stream and return the final [`ResultMessage`], if the run
    /// produced one. Stops at the first error.
    ///
    /// ```no_run
    /// # async fn run() -> claude_agent_sdk::Result<()> {
    /// let result = claude_agent_sdk::query("What is 2 + 2?", Default::default())
    ///     .await?
    ///     .final_result()
    ///     .await?;
    /// if let Some(result) = result {
    ///     println!("{}", result.result.unwrap_or_default());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn final_result(mut self) -> Result<Option<ResultMessage>> {
        let mut last = None;
        while let Some(message) = self.next_message().await {
            if let Message::Result(result) = message? {
                last = Some(result);
            }
        }
        Ok(last)
    }
}
