#![allow(missing_docs)] // self-describing DTO fields; types are documented above each definition

//! Parsed message types yielded by `query()` and the client.
//!
//! These mirror the Python SDK's content blocks and `Message` union, and all
//! implement `Serialize` so they can be forwarded as JSON (e.g. over HTTP). Every
//! struct keeps a typed view of the fields callers usually need, plus a
//! `raw`/`data`/`extra` escape hatch so nothing from the CLI is lost.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A block of assistant or user message content.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text(TextBlock),
    /// A thinking/reasoning block.
    Thinking(ThinkingBlock),
    /// A tool call.
    ToolUse(ToolUseBlock),
    /// A tool result (in user messages, or echoed in assistant messages).
    ToolResult(ToolResultBlock),
    /// A server-side tool call (advisor, web_search, ...).
    ServerToolUse(ServerToolUseBlock),
    /// The result of a server-side tool call.
    #[serde(rename = "advisor_tool_result")]
    ServerToolResult(ServerToolResultBlock),
    /// A block type this SDK version does not model, kept verbatim.
    #[serde(untagged)]
    Unknown(Value),
}

/// Text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextBlock {
    pub text: String,
}

/// Thinking content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: String,
}

/// Tool use content block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Tool result content block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    /// Result content: a string, a list of content blocks, or absent.
    pub content: Option<Value>,
    pub is_error: Option<bool>,
}

/// Server-side tool use block (e.g. `advisor`, `web_search`, `web_fetch`).
///
/// These are executed by the API; the caller never returns a result. Branch on
/// `name` to know which server tool was invoked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Result block returned for a server-side tool call. `content` is the raw
/// dict from the API.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerToolResultBlock {
    pub tool_use_id: String,
    pub content: Value,
}

impl ContentBlock {
    /// The block's text if it is a [`ContentBlock::Text`].
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(block) => Some(&block.text),
            _ => None,
        }
    }

    /// The tool call if it is a [`ContentBlock::ToolUse`].
    pub fn as_tool_use(&self) -> Option<&ToolUseBlock> {
        match self {
            ContentBlock::ToolUse(block) => Some(block),
            _ => None,
        }
    }
}

/// Provenance of a user-role message (and, on a [`ResultMessage`], of the
/// message that triggered the turn). Only `kind` is always present; other keys
/// depend on `kind`. Passed through from the CLI verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageOrigin {
    /// Discriminator, e.g. `human`, `channel`, `peer`, `task-notification`.
    pub kind: String,
    /// All fields, including `kind` and any not modeled here.
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

impl MessageOrigin {
    /// Whether the origin is a human prompt.
    pub fn is_human(&self) -> bool {
        self.kind == "human"
    }
}

/// A user message (a prompt, a tool result, or an injected turn).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserMessage {
    /// Either a plain string or a list of content blocks.
    pub content: UserContent,
    pub uuid: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub tool_use_result: Option<Value>,
    pub origin: Option<MessageOrigin>,
    pub session_id: Option<String>,
}

/// The content of a [`UserMessage`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum UserContent {
    /// A plain string.
    Text(String),
    /// A list of content blocks.
    Blocks(Vec<ContentBlock>),
}

/// An error category reported on an assistant message.
pub type AssistantMessageError = String;

/// An assistant message with content blocks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub parent_tool_use_id: Option<String>,
    pub error: Option<AssistantMessageError>,
    pub usage: Option<Value>,
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
    /// UUID of the user message this turn is answering.
    pub user_message_uuid: Option<String>,
    /// UUIDs of every user message this turn has taken in so far.
    pub user_message_uuids: Option<Vec<String>>,
}

/// A system message. `subtype` discriminates the payload; `data` is the raw
/// frame. For `subtype == "init"` see [`SystemMessage::init_data`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemMessage {
    pub subtype: String,
    pub data: Value,
}

impl SystemMessage {
    /// Convenience accessor for a field in `data`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// For an `init` message, the session metadata as a map.
    pub fn init_data(&self) -> Option<&serde_json::Map<String, Value>> {
        if self.subtype == "init" {
            self.data.as_object()
        } else {
            None
        }
    }
}

/// Per-model token usage and cost breakdown (camelCase, passed through from the
/// CLI's `modelUsage`).
pub type ModelUsage = Value;

/// A tool call deferred by a `PreToolUse` hook returning `defer`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeferredToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A result message with cost and usage information. Ends a turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultMessage {
    pub subtype: String,
    pub duration_ms: i64,
    pub duration_api_ms: i64,
    pub is_error: bool,
    pub num_turns: i64,
    pub session_id: String,
    pub stop_reason: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<Value>,
    /// Final assistant text, when the run produced one.
    pub result: Option<String>,
    /// Structured output, when `output_format` was set.
    pub structured_output: Option<Value>,
    pub model_usage: Option<Value>,
    pub permission_denials: Option<Vec<Value>>,
    pub deferred_tool_use: Option<DeferredToolUse>,
    pub errors: Option<Vec<String>>,
    pub api_error_status: Option<i64>,
    pub uuid: Option<String>,
    /// Why the query loop terminated (e.g. `completed`, `max_turns`,
    /// `aborted_streaming`).
    pub terminal_reason: Option<String>,
    pub origin: Option<MessageOrigin>,
    pub user_message_uuid: Option<String>,
    pub user_message_uuids: Option<Vec<String>>,
}

/// A partial-message stream event (emitted when `include_partial_messages` is
/// on). `event` is the raw Anthropic API stream event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamEvent {
    pub uuid: String,
    pub session_id: String,
    pub event: Value,
    pub parent_tool_use_id: Option<String>,
    pub user_message_uuid: Option<String>,
    pub user_message_uuids: Option<Vec<String>>,
}

/// Rate limit status emitted when the CLI's rate-limit state changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimitInfo {
    pub status: String,
    pub resets_at: Option<i64>,
    pub rate_limit_type: Option<String>,
    pub utilization: Option<f64>,
    pub overage_status: Option<String>,
    pub overage_resets_at: Option<i64>,
    pub overage_disabled_reason: Option<String>,
    /// Full raw dict, including fields not modeled above.
    pub raw: Value,
}

/// A rate limit event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: RateLimitInfo,
    pub uuid: String,
    pub session_id: String,
}

/// Emitted when the session's conversation is replaced without ending the
/// connection (e.g. after `/clear`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationResetMessage {
    pub new_conversation_id: String,
    pub uuid: String,
    pub session_id: String,
}

/// A message yielded to the consumer.
///
/// Forward compatible: a frame this SDK version does not model is yielded as
/// [`Message::Other`] rather than dropped, so newer CLI frames stay visible.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
// Messages are yielded and matched one at a time, not stored in bulk, so the
// size spread between the small and large variants does not matter here.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    System(SystemMessage),
    Result(ResultMessage),
    StreamEvent(StreamEvent),
    #[serde(rename = "rate_limit_event")]
    RateLimit(RateLimitEvent),
    ConversationReset(ConversationResetMessage),
    /// A frame whose `type` this SDK version does not model.
    #[serde(untagged)]
    Other(Value),
}

impl Message {
    /// Whether this is a [`Message::Result`].
    pub fn is_result(&self) -> bool {
        matches!(self, Message::Result(_))
    }

    /// The result message, if this is one.
    pub fn as_result(&self) -> Option<&ResultMessage> {
        match self {
            Message::Result(result) => Some(result),
            _ => None,
        }
    }

    /// The assistant message, if this is one.
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(message) => Some(message),
            _ => None,
        }
    }
}
