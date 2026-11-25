//! HTTP routes.
//!
//! | Method | Path                 | What it does                                   |
//! |--------|----------------------|------------------------------------------------|
//! | GET    | `/health`            | Liveness check (never requires auth)           |
//! | POST   | `/agent/run`         | Run a Claude Code agent, return the final result |
//! | POST   | `/agent/stream`      | Same, streaming every message as SSE           |
//! | POST   | `/messages`          | Direct Messages API passthrough                |
//! | POST   | `/messages/complete` | One prompt in, assistant text out              |
//!
//! Agent runs are driven by `claude_agent_sdk`; the server operator controls
//! the permission mode and whether clients may set `cwd`, so a caller can't
//! escalate what the agent is allowed to do.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use claude_agent_sdk::{ClaudeAgentOptions, ContentBlock, Message, query};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::MessagesClient;
use crate::config::Config;
use crate::error::ApiError;

/// Shared server state.
pub struct AppState {
    pub config: Config,
    pub messages: Option<MessagesClient>,
}

/// Build the router for `config`.
pub fn app(config: Config) -> Router {
    let messages = config
        .anthropic_api_key
        .as_ref()
        .map(|key| MessagesClient::new(key, &config.anthropic_base_url));
    let state = Arc::new(AppState { config, messages });

    let protected = Router::new()
        .route("/agent/run", post(run_agent))
        .route("/agent/stream", post(stream_agent))
        .route("/messages", post(messages_passthrough))
        .route("/messages/complete", post(messages_complete))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "agent-runtime" }))
}

/// Bearer-token check for everything but `/health`, active only when a token
/// is configured.
async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.config.auth_token {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if presented != Some(expected.as_str()) {
            return ApiError::unauthorized("missing or invalid bearer token").into_response();
        }
    }
    next.run(request).await
}

// ---------------------------------------------------------------------------
// Agent runs
// ---------------------------------------------------------------------------

/// Request body for `/agent/run` and `/agent/stream`.
#[derive(Debug, Deserialize)]
pub struct AgentRequest {
    /// The prompt to run.
    pub prompt: String,
    /// Model override (falls back to the server's `AGENT_MODEL`).
    #[serde(default)]
    pub model: Option<String>,
    /// A custom system prompt (replaces Claude Code's default).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Cap on conversation turns.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Tools allowed without prompting (falls back to the server default).
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Working directory; honored only when the server allows it.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Response body for `/agent/run`.
#[derive(Debug, Serialize)]
pub struct AgentRunResponse {
    pub session_id: String,
    /// The final assistant text (the result's own text, or the concatenated
    /// assistant turns when the result carries none).
    pub result: String,
    pub is_error: bool,
    pub subtype: String,
    pub num_turns: i64,
    pub duration_ms: i64,
    pub total_cost_usd: Option<f64>,
    /// How many messages the run produced.
    pub message_count: usize,
}

fn agent_options(state: &AppState, req: &AgentRequest) -> Result<ClaudeAgentOptions, ApiError> {
    if req.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    let config = &state.config;
    let mut builder = ClaudeAgentOptions::builder();
    if let Some(path) = &config.cli_path {
        builder = builder.cli_path(path.clone());
    }
    if let Some(model) = req.model.clone().or_else(|| config.default_model.clone()) {
        builder = builder.model(model);
    }
    if let Some(system_prompt) = &req.system_prompt {
        builder = builder.system_prompt(system_prompt.as_str());
    }
    if let Some(max_turns) = req.max_turns {
        builder = builder.max_turns(max_turns);
    }
    let tools = req
        .allowed_tools
        .clone()
        .unwrap_or_else(|| config.default_allowed_tools.clone());
    if !tools.is_empty() {
        builder = builder.allowed_tools(tools);
    }
    if let Some(mode) = config.permission_mode {
        builder = builder.permission_mode(mode);
    }
    if let Some(cwd) = &req.cwd {
        if !config.allow_cwd {
            return Err(ApiError::forbidden(
                "setting cwd is disabled on this server (AGENT_RUNTIME_ALLOW_CWD)",
            ));
        }
        builder = builder.cwd(cwd.clone());
    }
    Ok(builder.build())
}

async fn run_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentRequest>,
) -> Result<Json<AgentRunResponse>, ApiError> {
    let options = agent_options(&state, &req)?;
    let mut stream = query(req.prompt.as_str(), options).await?;

    let mut message_count = 0usize;
    let mut assistant_text = String::new();
    let mut result = None;
    while let Some(message) = stream.next_message().await {
        message_count += 1;
        match message? {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::Text(text) = block {
                        assistant_text.push_str(&text.text);
                    }
                }
            }
            Message::Result(r) => result = Some(r),
            _ => {}
        }
    }
    let Some(result) = result else {
        return Err(ApiError::upstream("the agent ended without a result"));
    };
    Ok(Json(AgentRunResponse {
        session_id: result.session_id,
        result: result.result.unwrap_or(assistant_text),
        is_error: result.is_error,
        subtype: result.subtype,
        num_turns: result.num_turns,
        duration_ms: result.duration_ms,
        total_cost_usd: result.total_cost_usd,
        message_count,
    }))
}

fn message_kind(message: &Message) -> &'static str {
    match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::System(_) => "system",
        Message::Result(_) => "result",
        Message::StreamEvent(_) => "stream_event",
        Message::RateLimit(_) => "rate_limit_event",
        Message::ConversationReset(_) => "conversation_reset",
        _ => "other",
    }
}

/// Stream every message of an agent run as Server-Sent Events. Each event is
/// named by message type and carries the message as JSON; the stream ends with
/// a `done` event (or an `error` event).
async fn stream_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let options = agent_options(&state, &req)?;
    let stream = query(req.prompt.as_str(), options).await?;

    let events = stream
        .map(|message| match message {
            Ok(message) => Event::default()
                .event(message_kind(&message))
                .json_data(&message)
                .unwrap_or_else(|e| Event::default().event("error").data(e.to_string())),
            Err(err) => Event::default().event("error").data(err.to_string()),
        })
        .chain(futures::stream::once(async {
            Event::default().event("done").data("")
        }))
        .map(Ok);

    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------
// Direct Messages API
// ---------------------------------------------------------------------------

fn messages_client(state: &AppState) -> Result<&MessagesClient, ApiError> {
    state.messages.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "direct Messages API routes need ANTHROPIC_API_KEY on the server",
        )
    })
}

/// Forward a Messages API request body as-is (with the server's key).
async fn messages_passthrough(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let response = messages_client(&state)?.create(&body).await?;
    Ok(Json(response))
}

/// Request body for `/messages/complete`.
#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    1024
}

/// One prompt in, the assistant's text out.
async fn messages_complete(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CompleteRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    let model = req
        .model
        .as_deref()
        .or(state.config.default_model.as_deref());
    let text = messages_client(&state)?
        .complete(model, &req.prompt, req.max_tokens)
        .await?;
    Ok(Json(json!({ "text": text })))
}
