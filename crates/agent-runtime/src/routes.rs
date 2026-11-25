//! HTTP routes.
//!
//! | Method | Path                   | What it does                                     |
//! |--------|------------------------|--------------------------------------------------|
//! | GET    | `/health`              | Liveness check (never requires auth)             |
//! | POST   | `/agent/run`           | Run one agent turn, return the final result      |
//! | POST   | `/agent/stream`        | Same, streaming every message as SSE             |
//! | GET    | `/agent/sessions`      | Kept sessions and current load                   |
//! | DELETE | `/agent/sessions/{id}` | Close a kept session                             |
//! | POST   | `/messages`            | Direct Messages API passthrough                  |
//! | POST   | `/messages/complete`   | One prompt in, assistant text out                |
//!
//! Agent runs are driven by `claude_agent_sdk`; the server operator controls
//! the permission mode and whether clients may set `cwd`, so a caller can't
//! escalate what the agent is allowed to do.
//!
//! # Capacity
//!
//! Every agent turn is a `claude` process of a few hundred MB, so the service
//! bounds them. At most `max_concurrent_runs` turns run at once (beyond that a
//! request gets `429` immediately rather than queueing), each turn is cut off
//! at `run_timeout` (`504`, process terminated), and a caller may keep a
//! session alive between turns (`keep_session`) so a follow-up skips the
//! process spawn and the `initialize` handshake and keeps the conversation.
//! Kept sessions are capped (`max_sessions`) and reclaimed after
//! `session_ttl` idle.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use claude_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient, ContentBlock, Message, ResultMessage};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::task::TaskTracker;

use crate::api::MessagesClient;
use crate::config::Config;
use crate::error::ApiError;
use crate::sessions::{Session, SessionPool};

/// Shared server state.
pub struct AppState {
    pub config: Config,
    pub messages: Option<MessagesClient>,
    /// Sessions kept alive between turns.
    pub pool: Arc<SessionPool>,
    /// One permit per turn that may run at once.
    run_slots: Arc<Semaphore>,
    /// Background tasks that own a `claude` process (streaming turns and
    /// process teardowns), waited on at shutdown so none is orphaned.
    tasks: TaskTracker,
}

impl AppState {
    /// Turns running right now.
    pub fn running(&self) -> usize {
        self.config
            .max_concurrent_runs
            .max(1)
            .saturating_sub(self.run_slots.available_permits())
    }

    /// Close every kept session and wait for in-flight background work, so no
    /// `claude` process outlives the server.
    pub async fn shutdown(&self) {
        self.pool.shutdown_all().await;
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// Build the router and the state behind it for `config`.
pub fn build(config: Config) -> (Router, Arc<AppState>) {
    let messages = config
        .anthropic_api_key
        .as_ref()
        .map(|key| MessagesClient::new(key, &config.anthropic_base_url));
    let pool = SessionPool::new(config.session_ttl, config.max_sessions);
    pool.spawn_reaper();
    let run_slots = Arc::new(Semaphore::new(config.max_concurrent_runs.max(1)));
    let state = Arc::new(AppState {
        config,
        messages,
        pool,
        run_slots,
        tasks: TaskTracker::new(),
    });

    let protected = Router::new()
        .route("/agent/run", post(run_agent))
        .route("/agent/stream", post(stream_agent))
        .route("/agent/sessions", get(list_sessions))
        .route("/agent/sessions/{id}", delete(close_session))
        .route("/messages", post(messages_passthrough))
        .route("/messages/complete", post(messages_complete))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    let router = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state.clone());
    (router, state)
}

/// Build just the router for `config`.
pub fn app(config: Config) -> Router {
    build(config).0
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
            .map(str::trim)
            .unwrap_or("");
        // Constant time: a plain `==` returns at the first differing byte,
        // which leaks how much of the token a guess got right.
        if !bool::from(presented.as_bytes().ct_eq(expected.as_bytes())) {
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
    /// Continue a kept session instead of starting a new process. A session's
    /// options are fixed when it starts, so the fields above are ignored on a
    /// follow-up turn.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Keep the session's process alive after this turn for follow-ups.
    /// Defaults to `true` when `session_id` is given, `false` otherwise.
    #[serde(default)]
    pub keep_session: Option<bool>,
}

impl AgentRequest {
    fn keep_session(&self) -> bool {
        self.keep_session.unwrap_or(self.session_id.is_some())
    }
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
    /// How many messages the turn produced.
    pub message_count: usize,
    /// Whether this turn ran in an existing kept session.
    pub session_reused: bool,
    /// Whether the session is still alive for a follow-up (pass `session_id`).
    pub session_kept: bool,
}

fn agent_options(state: &AppState, req: &AgentRequest) -> Result<ClaudeAgentOptions, ApiError> {
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

/// Where a turn's client came from.
enum Origin {
    /// A process started for this request (the shared handle is what becomes
    /// a [`Session`] if the caller keeps it).
    Fresh(Arc<Mutex<ClaudeSDKClient>>),
    /// A kept session, claimed for this turn.
    Pooled(Arc<Session>),
}

/// A claimed turn: the concurrency slot, the exclusively locked client, and
/// where it came from. Dropping it releases the slot (and, for a fresh
/// client, kills the process via the SDK), so no path can leak either.
struct Turn {
    permit: OwnedSemaphorePermit,
    client: OwnedMutexGuard<ClaudeSDKClient>,
    origin: Origin,
}

/// Validate the request and claim a slot and a client, cheapest checks first:
/// `400`/`403` for a bad request, `404` for an unknown session and `409` for
/// a busy one cost nothing, `429` is only ever returned to a request that
/// would otherwise have run, and the process spawn happens last.
async fn claim_turn(state: &AppState, req: &AgentRequest) -> Result<Turn, ApiError> {
    if req.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    enum Target {
        /// Boxed: the options struct dwarfs the other variant.
        Fresh(Box<ClaudeAgentOptions>),
        Existing(Arc<Session>, OwnedMutexGuard<ClaudeSDKClient>),
    }
    let target = match &req.session_id {
        Some(id) => {
            let session = state.pool.get(id).ok_or_else(|| {
                ApiError::new(StatusCode::NOT_FOUND, "unknown or expired session")
            })?;
            let client = session.try_lock().ok_or_else(|| {
                ApiError::new(StatusCode::CONFLICT, "session is busy with another turn")
            })?;
            Target::Existing(session, client)
        }
        None => Target::Fresh(Box::new(agent_options(state, req)?)),
    };
    let permit = state.run_slots.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many concurrent agent runs (limit {}); retry later",
                state.config.max_concurrent_runs
            ),
        )
    })?;
    match target {
        Target::Existing(session, client) => {
            session.touch();
            Ok(Turn {
                permit,
                client,
                origin: Origin::Pooled(session),
            })
        }
        Target::Fresh(options) => {
            let mut client = ClaudeSDKClient::new(*options);
            client.connect().await?;
            let shared = Arc::new(Mutex::new(client));
            let client = shared
                .clone()
                .try_lock_owned()
                .expect("a fresh mutex is unlocked");
            Ok(Turn {
                permit,
                client,
                origin: Origin::Fresh(shared),
            })
        }
    }
}

/// How a turn ended short of a result.
enum TurnError {
    Sdk(claude_agent_sdk::Error),
    /// The message stream ended without a result (the process died).
    Ended,
    TimedOut(Duration),
    /// The SSE consumer went away.
    ClientGone,
}

impl From<claude_agent_sdk::Error> for TurnError {
    fn from(err: claude_agent_sdk::Error) -> Self {
        TurnError::Sdk(err)
    }
}

impl TurnError {
    fn message(&self) -> String {
        match self {
            TurnError::Sdk(err) => err.to_string(),
            TurnError::Ended => "the agent ended without a result".to_string(),
            TurnError::TimedOut(limit) => format!(
                "the turn exceeded the run timeout ({}s) and its process was terminated",
                limit.as_secs()
            ),
            TurnError::ClientGone => "the client disconnected".to_string(),
        }
    }

    fn into_api_error(self) -> ApiError {
        let message = self.message();
        match self {
            TurnError::Sdk(err) => ApiError::from(err),
            TurnError::Ended => ApiError::upstream(message),
            TurnError::TimedOut(_) => ApiError::new(StatusCode::GATEWAY_TIMEOUT, message),
            TurnError::ClientGone => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message),
        }
    }
}

/// A frame of an SSE agent stream.
enum StreamFrame {
    /// Boxed: a message is far larger than the other variants.
    Message(Box<Message>),
    Error(String),
    Done {
        session_id: Option<String>,
        session_kept: bool,
    },
}

/// What a completed turn produced.
struct Progress {
    result: ResultMessage,
    assistant_text: String,
    message_count: usize,
}

struct TurnOutcome {
    progress: Progress,
    reused: bool,
    kept: bool,
}

impl TurnOutcome {
    fn into_body(self) -> AgentRunResponse {
        let Progress {
            result,
            assistant_text,
            message_count,
        } = self.progress;
        AgentRunResponse {
            session_id: result.session_id,
            result: result.result.unwrap_or(assistant_text),
            is_error: result.is_error,
            subtype: result.subtype,
            num_turns: result.num_turns,
            duration_ms: result.duration_ms,
            total_cost_usd: result.total_cost_usd,
            message_count,
            session_reused: self.reused,
            session_kept: self.kept,
        }
    }
}

/// Send the prompt and consume messages up to the result, forwarding each one
/// to `forward` when streaming.
async fn drive_turn(
    client: &mut ClaudeSDKClient,
    prompt: &str,
    forward: Option<&mpsc::Sender<StreamFrame>>,
) -> Result<Progress, TurnError> {
    client.query(prompt, "default").await?;
    let mut assistant_text = String::new();
    let mut message_count = 0usize;
    let mut responses = client.receive_response();
    loop {
        // When streaming, notice a consumer that hangs up right away (not at
        // the next message), so an abandoned turn frees its slot promptly.
        let next = match forward {
            Some(tx) => tokio::select! {
                next = responses.next() => next,
                _ = tx.closed() => return Err(TurnError::ClientGone),
            },
            None => responses.next().await,
        };
        let Some(message) = next else {
            break;
        };
        let message = message?;
        message_count += 1;
        if let Message::Assistant(assistant) = &message {
            for block in &assistant.content {
                if let ContentBlock::Text(text) = block {
                    assistant_text.push_str(&text.text);
                }
            }
        }
        let result = message.as_result().cloned();
        if let Some(tx) = forward
            && tx
                .send(StreamFrame::Message(Box::new(message)))
                .await
                .is_err()
        {
            return Err(TurnError::ClientGone);
        }
        if let Some(result) = result {
            return Ok(Progress {
                result,
                assistant_text,
                message_count,
            });
        }
    }
    Err(TurnError::Ended)
}

/// Run one turn on a claimed client under the run timeout, then keep or tear
/// down the session.
async fn execute_turn(
    state: &AppState,
    mut turn: Turn,
    prompt: &str,
    keep: bool,
    forward: Option<&mpsc::Sender<StreamFrame>>,
) -> Result<TurnOutcome, TurnError> {
    let reused = matches!(turn.origin, Origin::Pooled(_));
    let limit = state.config.run_timeout;
    // The borrow of `turn.client` ends with this statement, so `turn` can be
    // consumed below whatever happened.
    let outcome = tokio::time::timeout(limit, drive_turn(&mut turn.client, prompt, forward)).await;
    match outcome {
        Ok(Ok(progress)) => {
            let kept = settle(state, turn, keep, &progress.result.session_id);
            Ok(TurnOutcome {
                progress,
                reused,
                kept,
            })
        }
        Ok(Err(err)) => {
            evict(state, turn);
            Err(err)
        }
        Err(_elapsed) => {
            evict(state, turn);
            Err(TurnError::TimedOut(limit))
        }
    }
}

/// After a completed turn: keep the session for follow-ups or tear it down.
/// Returns whether it is still alive.
fn settle(state: &AppState, turn: Turn, keep: bool, session_id: &str) -> bool {
    let Turn {
        permit,
        client,
        origin,
    } = turn;
    if keep {
        match origin {
            Origin::Pooled(session) => {
                // Refresh the idle clock while still holding the lock, so the
                // reaper never sees a stale timestamp on an unlocked session.
                session.touch();
                drop(client);
                return true;
            }
            Origin::Fresh(shared) => {
                // Publish while still holding the lock: a request that finds
                // the session before we unlock just sees it busy for a moment.
                let session = Session::new(session_id.to_owned(), shared);
                if state.pool.insert(session).is_ok() {
                    drop(client);
                    return true;
                }
                tracing::warn!(
                    max = state.config.max_sessions,
                    "session pool is full; closing the session instead of keeping it"
                );
            }
        }
    } else if let Origin::Pooled(session) = &origin {
        state.pool.remove_exact(session);
    }
    teardown(state, permit, client);
    false
}

/// After an abnormal end (timeout, error, consumer gone) the session's state
/// is unknown: drop it from the pool and terminate its process.
fn evict(state: &AppState, turn: Turn) {
    let Turn {
        permit,
        client,
        origin,
    } = turn;
    if let Origin::Pooled(session) = &origin {
        state.pool.remove_exact(session);
    }
    teardown(state, permit, client);
}

/// Terminate a process in the background, holding its concurrency slot until
/// it is gone, so the response needn't wait for the exit but the memory bound
/// stays honest.
fn teardown(
    state: &AppState,
    permit: OwnedSemaphorePermit,
    mut client: OwnedMutexGuard<ClaudeSDKClient>,
) {
    state.tasks.spawn(async move {
        client.disconnect().await;
        drop(permit);
    });
}

async fn run_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentRequest>,
) -> Result<Json<AgentRunResponse>, ApiError> {
    let turn = claim_turn(&state, &req).await?;
    let outcome = execute_turn(&state, turn, &req.prompt, req.keep_session(), None)
        .await
        .map_err(TurnError::into_api_error)?;
    Ok(Json(outcome.into_body()))
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

fn error_event(message: &str) -> Event {
    match Event::default()
        .event("error")
        .json_data(json!({ "error": message }))
    {
        Ok(event) => event,
        Err(_) => Event::default().event("error").data(message),
    }
}

impl StreamFrame {
    fn into_event(self) -> Event {
        match self {
            StreamFrame::Message(message) => Event::default()
                .event(message_kind(&message))
                .json_data(&message)
                .unwrap_or_else(|e| error_event(&e.to_string())),
            StreamFrame::Error(message) => error_event(&message),
            StreamFrame::Done {
                session_id,
                session_kept,
            } => Event::default()
                .event("done")
                .json_data(json!({ "session_id": session_id, "session_kept": session_kept }))
                .unwrap_or_else(|e| error_event(&e.to_string())),
        }
    }
}

/// Stream every message of an agent turn as Server-Sent Events. Each event is
/// named by message type and carries the message as JSON. A failure is an
/// `error` event; the stream always ends with a `done` event carrying the
/// session id and whether the session was kept.
///
/// Request validation and the process spawn happen before the response
/// starts, so `400`/`404`/`409`/`429`/`502` arrive as HTTP statuses. If the
/// consumer disconnects mid-turn the turn is abandoned and its process
/// terminated (a kept session can't be trusted half-consumed).
async fn stream_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let turn = claim_turn(&state, &req).await?;
    let keep = req.keep_session();
    let prompt = req.prompt;
    // Bounded: a slow consumer applies backpressure to the turn (and through
    // the SDK's byte budget to the process) instead of buffering it all.
    let (tx, mut rx) = mpsc::channel::<StreamFrame>(32);
    let worker_state = state.clone();
    state.tasks.spawn(async move {
        match execute_turn(&worker_state, turn, &prompt, keep, Some(&tx)).await {
            Ok(outcome) => {
                let _ = tx
                    .send(StreamFrame::Done {
                        session_id: Some(outcome.progress.result.session_id),
                        session_kept: outcome.kept,
                    })
                    .await;
            }
            Err(TurnError::ClientGone) => {}
            Err(err) => {
                let _ = tx.send(StreamFrame::Error(err.message())).await;
                let _ = tx
                    .send(StreamFrame::Done {
                        session_id: None,
                        session_kept: false,
                    })
                    .await;
            }
        }
    });

    let events =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(|frame| Ok(frame.into_event()));
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Kept sessions and current load.
async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Value> {
    let sessions: Vec<Value> = state
        .pool
        .snapshot()
        .into_iter()
        .map(|(id, idle)| json!({ "id": id, "idle_secs": idle.as_secs() }))
        .collect();
    Json(json!({
        "count": sessions.len(),
        "max_sessions": state.config.max_sessions,
        "running": state.running(),
        "max_concurrent_runs": state.config.max_concurrent_runs,
        "sessions": sessions,
    }))
}

/// Close a kept session now (its process exits). `409` while a turn is running
/// on it.
async fn close_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session = state
        .pool
        .get(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown or expired session"))?;
    let mut client = session.try_lock().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "session is busy with another turn; close it when that turn finishes",
        )
    })?;
    state.pool.remove_exact(&session);
    client.disconnect().await;
    Ok(StatusCode::NO_CONTENT)
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
