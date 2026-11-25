//! [`ClaudeSDKClient`]: a bidirectional, stateful Claude Code session.
//!
//! Unlike [`query`](crate::query), which is one-shot, the client keeps the
//! connection open so you can send follow-up prompts, interrupt the current
//! turn, and change the model or permission mode mid-conversation. Ports the
//! Python SDK's `ClaudeSDKClient`.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::errors::{Error, Result};
use crate::message_parser::parse_message;
use crate::query::{InputHandle, Query, Queued};
use crate::transport::{SubprocessTransport, Transport};
use crate::types::{ClaudeAgentOptions, Message, PermissionMode};

/// A client for interactive, multi-turn conversations with Claude Code.
///
/// ```no_run
/// use claude_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
/// use futures::StreamExt;
///
/// # async fn run() -> claude_agent_sdk::Result<()> {
/// let mut client = ClaudeSDKClient::new(ClaudeAgentOptions::default());
/// client.connect().await?;
/// client.query("Add a test for the parser", "default").await?;
/// let mut responses = client.receive_response();
/// while let Some(message) = responses.next().await {
///     println!("{:?}", message?);
/// }
/// drop(responses);
/// client.disconnect().await;
/// # Ok(())
/// # }
/// ```
pub struct ClaudeSDKClient {
    options: Arc<ClaudeAgentOptions>,
    engine: Option<Query>,
    input: Option<InputHandle>,
    rx: Option<mpsc::UnboundedReceiver<Result<Queued>>>,
}

impl ClaudeSDKClient {
    /// Create a client. Call [`connect`](Self::connect) before sending.
    pub fn new(options: ClaudeAgentOptions) -> Self {
        ClaudeSDKClient {
            options: Arc::new(options),
            engine: None,
            input: None,
            rx: None,
        }
    }

    /// Connect with no initial prompt (an interactive session). Equivalent to
    /// `connect_with(None)`.
    pub async fn connect(&mut self) -> Result<()> {
        self.connect_inner(None).await
    }

    /// Connect and send an initial string prompt.
    pub async fn connect_with(&mut self, prompt: impl Into<String>) -> Result<()> {
        self.connect_inner(Some(prompt.into())).await
    }

    /// Connect using a caller-supplied [`Transport`] instead of spawning the
    /// `claude` CLI. See [`query_with_transport`](crate::query_with_transport).
    pub async fn connect_transport<T: Transport + 'static>(
        &mut self,
        transport: T,
        prompt: Option<String>,
    ) -> Result<()> {
        let options = self.configured_options()?;
        self.start_with(transport, prompt, options).await
    }

    async fn connect_inner(&mut self, prompt: Option<String>) -> Result<()> {
        // Configure a copy per connect (never mutate `self.options`, so a
        // reconnect sees the user's original settings): this routes permission
        // prompts over stdio when `can_use_tool` is set, before the transport
        // turns the options into CLI flags.
        let options = self.configured_options()?;
        let transport = SubprocessTransport::new(options.clone());
        self.start_with(transport, prompt, options).await
    }

    fn configured_options(&self) -> Result<Arc<ClaudeAgentOptions>> {
        Ok(Arc::new(
            (*self.options).clone().configured_for_can_use_tool()?,
        ))
    }

    async fn start_with<T: Transport + 'static>(
        &mut self,
        mut transport: T,
        prompt: Option<String>,
        options: Arc<ClaudeAgentOptions>,
    ) -> Result<()> {
        transport.connect().await?;
        let stream = transport
            .take_message_stream()
            .ok_or_else(|| Error::cli_connection("transport produced no message stream"))?;
        let transport: Arc<dyn Transport> = Arc::new(transport);

        let mut engine = Query::new(transport, &options);
        engine.start(stream);
        engine.initialize(crate::initialize_timeout()).await?;

        self.input = Some(engine.input_handle());
        self.rx = engine.take_messages();
        if let Some(prompt) = prompt {
            engine.write_user_message(&prompt, "default").await?;
        }
        self.engine = Some(engine);
        Ok(())
    }

    /// Send a new user message (string) to the session.
    pub async fn query(&self, prompt: impl AsRef<str>, session_id: impl AsRef<str>) -> Result<()> {
        let engine = self.engine()?;
        engine
            .write_user_message(prompt.as_ref(), session_id.as_ref())
            .await
    }

    /// Send a raw user-message JSON value (streaming input shape).
    pub async fn query_value(&self, message: Value) -> Result<()> {
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| Error::cli_connection("Not connected. Call connect() first."))?;
        input.write(message).await
    }

    /// Interrupt the current turn.
    pub async fn interrupt(&self) -> Result<()> {
        self.engine()?.interrupt().await
    }

    /// Change the permission mode mid-conversation.
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.engine()?.set_permission_mode(mode).await
    }

    /// Change the model mid-conversation (`None` resets to the default).
    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.engine()?.set_model(model).await
    }

    /// Get the MCP server connection status.
    pub async fn mcp_status(&self) -> Result<crate::types::McpStatusResponse> {
        let value = self.engine()?.mcp_status().await?;
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    /// Get the raw context-usage breakdown (as the `/context` command shows).
    pub async fn context_usage(&self) -> Result<Value> {
        self.engine()?.context_usage().await
    }

    /// Reconnect a disconnected or failed MCP server.
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> Result<()> {
        self.engine()?.reconnect_mcp_server(server_name).await
    }

    /// Enable or disable an MCP server.
    pub async fn toggle_mcp_server(&self, server_name: &str, enabled: bool) -> Result<()> {
        self.engine()?.toggle_mcp_server(server_name, enabled).await
    }

    /// Rewind tracked files to the state at a user message (requires
    /// `enable_file_checkpointing`).
    pub async fn rewind_files(&self, user_message_id: &str) -> Result<()> {
        self.engine()?.rewind_files(user_message_id).await
    }

    /// Stop a running background task.
    pub async fn stop_task(&self, task_id: &str) -> Result<()> {
        self.engine()?.stop_task(task_id).await
    }

    /// Stream every message from the session until the connection ends.
    ///
    /// Borrows the client mutably for the lifetime of the stream. Drop the
    /// returned stream to send another query or call a control method.
    pub fn receive_messages(&mut self) -> MessageBorrow<'_> {
        MessageBorrow {
            rx: self.rx.as_mut(),
            stop_after_result: false,
            done: false,
        }
    }

    /// Stream messages until (and including) the next [`Message::Result`], then
    /// stop. Convenient for one request/response exchange.
    pub fn receive_response(&mut self) -> MessageBorrow<'_> {
        MessageBorrow {
            rx: self.rx.as_mut(),
            stop_after_result: true,
            done: false,
        }
    }

    /// Disconnect and tear down the subprocess. Safe to call more than once.
    pub async fn disconnect(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            engine.close().await;
        }
        self.input = None;
        self.rx = None;
    }

    /// Whether the session is connected and its CLI process is still running.
    ///
    /// `false` before `connect`, after `disconnect`, and as soon as the process
    /// has exited or closed its output (its message stream has ended), which
    /// makes this a cheap liveness check before reusing an idle client. It
    /// does not send anything to the process.
    pub fn is_connected(&self) -> bool {
        // The engine's reader task is the only holder of the channel's sender;
        // it ends, dropping the sender, exactly when the stream ends.
        self.engine.is_some() && self.rx.as_ref().is_some_and(|rx| !rx.is_closed())
    }

    /// The CLI's `initialize` response (available commands, output styles,
    /// models, account info, ...), or `None` before `connect`.
    pub fn server_info(&self) -> Option<Value> {
        self.engine.as_ref().and_then(Query::server_info)
    }

    fn engine(&self) -> Result<&Query> {
        self.engine
            .as_ref()
            .ok_or_else(|| Error::cli_connection("Not connected. Call connect() first."))
    }
}

impl Drop for ClaudeSDKClient {
    fn drop(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            // See `EngineGuard::drop`: never `tokio::spawn` without a runtime.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    engine.close().await;
                });
            }
        }
    }
}

/// A borrowed message stream from [`ClaudeSDKClient::receive_messages`] or
/// [`ClaudeSDKClient::receive_response`].
pub struct MessageBorrow<'a> {
    rx: Option<&'a mut mpsc::UnboundedReceiver<Result<Queued>>>,
    stop_after_result: bool,
    done: bool,
}

impl Stream for MessageBorrow<'_> {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(None);
        };
        match rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(queued))) => {
                let parsed = parse_message(queued.into_value());
                if self.stop_after_result {
                    if let Ok(Message::Result(_)) = &parsed {
                        self.done = true;
                    }
                }
                Poll::Ready(Some(parsed))
            }
            Poll::Ready(Some(Err(err))) => {
                self.done = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
