//! The bidirectional control protocol, layered over a [`Transport`].
//!
//! [`Query`] owns the reader task that routes frames from the CLI: control
//! responses go to the request that is waiting for them, control requests
//! (`can_use_tool`, `hook_callback`, `mcp_message`) are dispatched to the
//! configured callbacks and answered, and everything else is forwarded to the
//! consumer's message stream. It also runs the "when can stdin be closed"
//! logic that keeps the control channel open while hooks, SDK MCP servers or a
//! permission callback might still be called (ports the Python SDK's `Query`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use std::sync::Mutex as StdMutex;

use tokio::sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::errors::{Error, Result, ResultError};
use crate::mcp::SdkMcpServer;
use crate::transport::Transport;
use crate::types::{
    CanUseTool, ClaudeAgentOptions, HookCallback, HookContext, PermissionResult,
    ToolPermissionContext,
};

/// Task types whose completion runs a follow-up turn; a result that arrives
/// while one is in flight must not close stdin (mirrors the Python SDK).
const DEFERRING_TASK_TYPES: &[&str] = &["local_agent", "local_workflow"];

/// Terminal task statuses that clear an in-flight task.
const TERMINAL_TASK_STATUSES: &[&str] = &["completed", "failed", "stopped", "killed"];

/// Default ceiling for how long the run stays open past a result with no new
/// turn while the CLI still reports work (10 minutes).
const DEFAULT_RUN_END_CEILING: Duration = Duration::from_secs(600);

/// Bytes of decoded *data* frames that may sit between the router and the
/// consumer before the router applies backpressure (stops reading the
/// transport).
///
/// Control frames (the CLI's `can_use_tool` / hook / MCP requests, and the
/// responses to our own `interrupt` / `set_model` ...) never count against
/// this and never wait behind data, so a slow consumer can only delay a control
/// response once this much data is unconsumed — not after a fixed handful of
/// frames. Bounding by bytes rather than frame count is what keeps memory
/// predictable when frame sizes range from 40 B stream events to multi-MB tool
/// results.
const DATA_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// A data frame queued for the consumer, holding the byte budget it occupies.
///
/// Yielded by [`Query::take_messages`]. Dropping it, or calling
/// [`into_value`](Self::into_value), returns its share of the budget so the
/// router may read further frames.
pub struct Queued {
    value: Value,
    _permit: Option<OwnedSemaphorePermit>,
}

impl Queued {
    /// Take the raw frame, releasing its budget.
    pub fn into_value(self) -> Value {
        self.value
    }
}

/// Cheap O(nodes) size estimate of a JSON value, used to charge data frames
/// against the byte budget without re-serializing them. Summing string lengths
/// is O(1) per node, so this costs far less than the parse that produced it.
fn approx_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => 8,
        Value::String(s) => 24 + s.len(),
        Value::Array(items) => 24 + items.iter().map(approx_size).sum::<usize>(),
        Value::Object(map) => {
            24 + map
                .iter()
                .map(|(k, v)| 24 + k.len() + approx_size(v))
                .sum::<usize>()
        }
    }
}

/// One registered hook callback with its matcher metadata.
struct HookRegistration {
    event: String,
    matcher: Option<String>,
    timeout: Option<f64>,
    on_failure: Option<String>,
    callback_ids: Vec<String>,
}

/// Internal state shared between the public [`Query`] handle and its reader
/// task.
struct Shared {
    transport: Arc<dyn Transport>,
    can_use_tool: Option<CanUseTool>,
    hook_callbacks: HashMap<String, HookCallback>,
    hook_registrations: Vec<HookRegistration>,
    sdk_mcp_servers: HashMap<String, Arc<SdkMcpServer>>,
    pending: StdMutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    request_counter: AtomicU64,
    // A std (non-async) mutex: every critical section is tiny and synchronous,
    // and this is taken on the message hot path, so an async mutex (with its
    // await point and scheduler interaction) would be pure overhead.
    run: StdMutex<RunState>,
    run_ended: Notify,
    /// Byte budget for data frames queued to the consumer (see
    /// [`DATA_BUDGET_BYTES`]).
    data_budget: Arc<Semaphore>,
    has_bidirectional_needs: bool,
    /// The CLI's `initialize` response, kept for `server_info`.
    init_result: StdMutex<Option<Value>>,
    /// In-flight handlers for the CLI's control requests, by request id, so a
    /// `control_cancel_request` can abort one and `close` can abort them all.
    inflight: StdMutex<HashMap<String, tokio::task::AbortHandle>>,
    // initialize payload
    agents: Option<Value>,
    exclude_dynamic_sections: Option<bool>,
    system_prompt_snapshot: Option<bool>,
    skills: Option<Vec<String>>,
    forward_subagent_text: bool,
}

#[derive(Default)]
struct RunState {
    result_received: bool,
    run_final: bool,
    ended: bool,
    session_state: Option<String>,
    inflight_tasks: std::collections::HashSet<String>,
    last_error_result: Option<Value>,
}

/// Handle to a running control-protocol session.
pub struct Query {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    messages: Option<mpsc::UnboundedReceiver<Result<Queued>>>,
    verbatim_prompts: bool,
}

impl Query {
    /// Build a query over `transport`, wiring the callbacks and initialize
    /// payload from `options`. Call [`start`](Query::start) to begin reading.
    pub fn new(transport: Arc<dyn Transport>, options: &ClaudeAgentOptions) -> Self {
        let (hook_callbacks, hook_registrations) = build_hook_registry(options);
        let sdk_mcp_servers = options
            .mcp_servers
            .as_ref()
            .map(|servers| servers.sdk_servers())
            .unwrap_or_default();
        let agents = options.agents.as_ref().map(|agents| {
            let map: serde_json::Map<String, Value> = agents
                .iter()
                .map(|(name, def)| {
                    (
                        name.clone(),
                        serde_json::to_value(def).unwrap_or(Value::Null),
                    )
                })
                .collect();
            Value::Object(map)
        });
        let (exclude_dynamic_sections, system_prompt_snapshot) =
            system_prompt_flags(&options.system_prompt);
        let skills = match &options.skills {
            Some(crate::types::Skills::List(names)) => Some(names.clone()),
            _ => None,
        };
        let shared = Arc::new(Shared {
            transport,
            can_use_tool: options.can_use_tool.clone(),
            hook_callbacks,
            hook_registrations,
            sdk_mcp_servers,
            pending: StdMutex::new(HashMap::new()),
            request_counter: AtomicU64::new(0),
            run: StdMutex::new(RunState::default()),
            run_ended: Notify::new(),
            data_budget: Arc::new(Semaphore::new(DATA_BUDGET_BYTES)),
            has_bidirectional_needs: options.has_bidirectional_needs(),
            init_result: StdMutex::new(None),
            inflight: StdMutex::new(HashMap::new()),
            agents,
            exclude_dynamic_sections,
            system_prompt_snapshot,
            skills,
            forward_subagent_text: options.forward_subagent_text,
        });
        Query {
            shared,
            reader: None,
            messages: None,
            verbatim_prompts: options.verbatim_prompts,
        }
    }

    /// Start reading frames from the transport. Takes the transport's message
    /// stream and spawns the router task.
    pub fn start(&mut self, mut stream: crate::transport::MessageStream) {
        // Unbounded in *count*: memory is bounded by the byte budget each
        // queued frame holds (see `DATA_BUDGET_BYTES`), which is the right
        // unit, rather than by a frame count that a few huge frames would make
        // meaningless.
        let (tx, rx) = mpsc::unbounded_channel::<Result<Queued>>();
        self.messages = Some(rx);
        let shared = self.shared.clone();
        self.reader = Some(tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(value) => {
                        if !route_frame(&shared, value, &tx).await {
                            break;
                        }
                    }
                    Err(err) => {
                        // Replace a bare process error with a ResultError when
                        // the last result said why it failed.
                        let err = enrich_error(&shared, err).await;
                        // Fail any control request still waiting (an
                        // `initialize` in flight when the CLI dies, say) with
                        // the same error, instead of letting it time out.
                        fail_pending(&shared, &err);
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
            // Stream ended: nothing can answer a pending control request now.
            fail_pending(
                &shared,
                &Error::cli_connection(
                    "the CLI closed its output before answering the control request",
                ),
            );
            // Mark the run final and wake any closer.
            {
                let mut run = shared.run.lock().unwrap();
                run.run_final = true;
                run.ended = true;
            }
            shared.run_ended.notify_waiters();
        }));
    }

    /// The initialize handshake. Sends the hooks/agents/skills payload and
    /// returns the CLI's initialize response.
    pub async fn initialize(&self, timeout: Duration) -> Result<Value> {
        let mut request = serde_json::Map::new();
        request.insert("subtype".into(), json!("initialize"));

        // Build the hooks wire config from the registrations.
        if !self.shared.hook_registrations.is_empty() {
            let mut hooks_config: serde_json::Map<String, Value> = serde_json::Map::new();
            for reg in &self.shared.hook_registrations {
                let mut matcher_cfg = serde_json::Map::new();
                matcher_cfg.insert(
                    "matcher".into(),
                    reg.matcher
                        .as_ref()
                        .map(|m| json!(m))
                        .unwrap_or(Value::Null),
                );
                matcher_cfg.insert("hookCallbackIds".into(), json!(reg.callback_ids));
                if let Some(timeout) = reg.timeout {
                    matcher_cfg.insert("timeout".into(), json!(timeout));
                }
                if let Some(on_failure) = &reg.on_failure {
                    matcher_cfg.insert("onFailure".into(), json!(on_failure));
                }
                hooks_config
                    .entry(reg.event.clone())
                    .or_insert_with(|| Value::Array(vec![]))
                    .as_array_mut()
                    .unwrap()
                    .push(Value::Object(matcher_cfg));
            }
            request.insert("hooks".into(), Value::Object(hooks_config));
        } else {
            request.insert("hooks".into(), Value::Null);
        }

        if let Some(agents) = &self.shared.agents {
            request.insert("agents".into(), agents.clone());
        }
        if let Some(exclude) = self.shared.exclude_dynamic_sections {
            request.insert("excludeDynamicSections".into(), json!(exclude));
        }
        if let Some(snapshot) = self.shared.system_prompt_snapshot {
            request.insert("systemPromptSnapshot".into(), json!(snapshot));
        }
        if let Some(skills) = &self.shared.skills {
            request.insert("skills".into(), json!(skills));
        }
        if self.shared.forward_subagent_text {
            request.insert("forwardSubagentText".into(), json!(true));
        }

        let response = self
            .shared
            .send_control_request(Value::Object(request), timeout)
            .await?;

        // A matcher set to fail *closed* must not silently fail open against a
        // CLI that doesn't honor `onFailure`, or one that didn't register the
        // hooks at all (mirrors the Python SDK's guard).
        let blocks_on_failure = self
            .shared
            .hook_registrations
            .iter()
            .any(|r| r.on_failure.as_deref() == Some("block"));
        if blocks_on_failure {
            if response
                .get("hook_on_failure_supported")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err(Error::cli_connection(
                    "The process answering initialize does not honor on_failure on hook \
                     matchers, so a hook that fails would not block. Update Claude Code if it \
                     is old, or remove on_failure=Block from your hooks.",
                ));
            }
            if response.get("hooks_applied").and_then(Value::as_bool) == Some(false) {
                return Err(Error::cli_connection(
                    "This session was already running and did not register your hooks, so a \
                     hook set to on_failure=Block would never run. Start a new session, or \
                     remove on_failure=Block from your hooks.",
                ));
            }
        }

        *self.shared.init_result.lock().unwrap() = Some(response.clone());
        Ok(response)
    }

    /// The CLI's `initialize` response (commands, output styles, models,
    /// account, ...), or `None` before [`initialize`](Self::initialize)
    /// completed.
    pub fn server_info(&self) -> Option<Value> {
        self.shared.init_result.lock().unwrap().clone()
    }

    /// Write a user-message frame (a string prompt) to the CLI.
    pub async fn write_user_message(&self, prompt: &str, session_id: &str) -> Result<()> {
        let mut message = json!({
            "type": "user",
            "message": {"role": "user", "content": prompt},
            "parent_tool_use_id": Value::Null,
            "session_id": session_id,
        });
        if self.verbatim_prompts {
            message["client_composed"] = json!(true);
        }
        self.shared.transport.write(format!("{message}\n")).await
    }

    /// Write a raw user-message dict (streaming-input mode).
    pub async fn write_message_value(&self, mut message: Value) -> Result<()> {
        if self.verbatim_prompts {
            if let Some(obj) = message.as_object_mut() {
                obj.insert("client_composed".into(), json!(true));
            }
        }
        self.shared.transport.write(format!("{message}\n")).await
    }

    /// Wait for the run to end (if bidirectional needs keep stdin open) and
    /// close the input side. Spawn this after sending a one-shot prompt.
    pub fn close_input_when_done(&self) {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            if shared.has_bidirectional_needs {
                // Register interest *before* reading `ended`, so a run that
                // ends between the check and the await still wakes us (a plain
                // `notify_waiters` leaves no permit for a late waiter).
                let notified = shared.run_ended.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !shared.run.lock().unwrap().ended {
                    let _ = tokio::time::timeout(DEFAULT_RUN_END_CEILING, &mut notified).await;
                }
            }
            shared.run.lock().unwrap().run_final = true;
            let _ = shared.transport.end_input().await;
        });
    }

    /// Take the consumer message stream. Call once, after `start`.
    pub fn take_messages(&mut self) -> Option<mpsc::UnboundedReceiver<Result<Queued>>> {
        self.messages.take()
    }

    /// A cloneable handle for streaming user messages into this session from a
    /// separate task (used by [`query`](crate::query) in streaming-input mode
    /// and by [`ClaudeSDKClient`](crate::ClaudeSDKClient)).
    pub fn input_handle(&self) -> InputHandle {
        InputHandle {
            shared: self.shared.clone(),
            verbatim: self.verbatim_prompts,
        }
    }

    /// Send an interrupt control request.
    pub async fn interrupt(&self) -> Result<()> {
        self.simple_control("interrupt").await
    }

    /// Change the permission mode.
    pub async fn set_permission_mode(&self, mode: crate::types::PermissionMode) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "set_permission_mode", "mode": mode.as_str()}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    /// Change the model.
    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "set_model", "model": model}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    /// Get the MCP server connection status.
    pub async fn mcp_status(&self) -> Result<Value> {
        self.shared
            .send_control_request(json!({"subtype": "mcp_status"}), default_control_timeout())
            .await
    }

    /// Get the context-window usage breakdown.
    pub async fn context_usage(&self) -> Result<Value> {
        self.shared
            .send_control_request(
                json!({"subtype": "get_context_usage"}),
                default_control_timeout(),
            )
            .await
    }

    /// Rewind tracked files to the state at `user_message_id`.
    pub async fn rewind_files(&self, user_message_id: &str) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "rewind_files", "user_message_id": user_message_id}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    /// Reconnect a disconnected or failed MCP server.
    pub async fn reconnect_mcp_server(&self, server_name: &str) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "mcp_reconnect", "serverName": server_name}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    /// Enable or disable an MCP server.
    pub async fn toggle_mcp_server(&self, server_name: &str, enabled: bool) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "mcp_toggle", "serverName": server_name, "enabled": enabled}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    /// Stop a running task.
    pub async fn stop_task(&self, task_id: &str) -> Result<()> {
        self.shared
            .send_control_request(
                json!({"subtype": "stop_task", "task_id": task_id}),
                default_control_timeout(),
            )
            .await
            .map(|_| ())
    }

    async fn simple_control(&self, subtype: &str) -> Result<()> {
        self.shared
            .send_control_request(json!({ "subtype": subtype }), default_control_timeout())
            .await
            .map(|_| ())
    }

    /// Close the query: stop the reader and close the transport.
    pub async fn close(&mut self) {
        // Stop any control handlers still running (a slow permission callback,
        // say); their responses have nowhere to go.
        for (_, handle) in self.shared.inflight.lock().unwrap().drain() {
            handle.abort();
        }
        // Fail any still-pending control requests.
        {
            let mut pending = self.shared.pending.lock().unwrap();
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(Error::cli_connection("query closed")));
            }
        }
        let _ = self.shared.transport.close().await;
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// A cloneable input side of a [`Query`]: write user messages and close input.
#[derive(Clone)]
pub struct InputHandle {
    shared: Arc<Shared>,
    verbatim: bool,
}

impl InputHandle {
    /// Write one user-message JSON value, stamping `client_composed` when
    /// verbatim prompts are on and defaulting `session_id` to `default`.
    pub async fn write(&self, mut message: Value) -> Result<()> {
        if let Some(obj) = message.as_object_mut() {
            if self.verbatim {
                obj.insert("client_composed".into(), json!(true));
            }
            obj.entry("session_id").or_insert_with(|| json!("default"));
        }
        self.shared.transport.write(format!("{message}\n")).await
    }

    /// After the last message, wait for the run to end (when bidirectional
    /// needs keep stdin open) and close the input side.
    pub async fn finish(&self, wrote: bool) {
        if wrote && self.shared.has_bidirectional_needs {
            let notified = self.shared.run_ended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.shared.run.lock().unwrap().ended {
                let _ = tokio::time::timeout(DEFAULT_RUN_END_CEILING, &mut notified).await;
            }
        }
        self.shared.run.lock().unwrap().run_final = true;
        let _ = self.shared.transport.end_input().await;
    }
}

impl Shared {
    async fn send_control_request(&self, request: Value, timeout: Duration) -> Result<Value> {
        let id = self.request_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let request_id = format!("req_{id}_{:08x}", rand_u32());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id.clone(), tx);

        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if let Err(err) = self.transport.write(format!("{frame}\n")).await {
            self.pending.lock().unwrap().remove(&request_id);
            return Err(err);
        }

        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending.lock().unwrap().remove(&request_id);
                Err(Error::control("control request channel closed"))
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&request_id);
                Err(Error::ControlTimeout(subtype))
            }
        }
    }
}

/// Resolve every pending control request with `err` (cloned), so callers fail
/// fast instead of waiting out their timeout once the CLI is gone.
fn fail_pending(shared: &Arc<Shared>, err: &Error) {
    let pending: Vec<_> = shared.pending.lock().unwrap().drain().collect();
    for (_, sender) in pending {
        let _ = sender.send(Err(err.clone()));
    }
}

/// Route one frame. Returns `false` when the reader should stop.
async fn route_frame(
    shared: &Arc<Shared>,
    message: Value,
    tx: &mpsc::UnboundedSender<Result<Queued>>,
) -> bool {
    let msg_type = message.get("type").and_then(Value::as_str).unwrap_or("");

    match msg_type {
        "control_response" => {
            let response = message.get("response").cloned().unwrap_or(Value::Null);
            let request_id = response
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(request_id) = request_id {
                let sender = shared.pending.lock().unwrap().remove(&request_id);
                if let Some(sender) = sender {
                    let result = if response.get("subtype").and_then(Value::as_str) == Some("error")
                    {
                        Err(Error::control(
                            response
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown error")
                                .to_string(),
                        ))
                    } else {
                        Ok(response.get("response").cloned().unwrap_or(json!({})))
                    };
                    let _ = sender.send(result);
                }
            }
            return true;
        }
        "control_request" => {
            let request_id = message
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let shared_task = shared.clone();
            let transport = shared.transport.clone();
            let id_for_task = request_id.clone();
            // Hold the in-flight map across the spawn so a handler that
            // finishes instantly can't remove its entry before it is inserted.
            let mut inflight = shared.inflight.lock().unwrap();
            let handle = tokio::spawn(async move {
                let response = handle_control_request(&shared_task, &message).await;
                let frame = match response {
                    Ok(data) => json!({
                        "type": "control_response",
                        "response": {"subtype": "success", "request_id": id_for_task, "response": data},
                    }),
                    Err(err) => json!({
                        "type": "control_response",
                        "response": {"subtype": "error", "request_id": id_for_task, "error": err.to_string()},
                    }),
                };
                let _ = transport.write(format!("{frame}\n")).await;
                shared_task.inflight.lock().unwrap().remove(&id_for_task);
            });
            inflight.insert(request_id, handle.abort_handle());
            return true;
        }
        "control_cancel_request" => {
            // The CLI abandoned this request: stop the handler so it neither
            // does wasted work nor writes a response nobody is waiting for.
            if let Some(id) = message.get("request_id").and_then(Value::as_str) {
                if let Some(handle) = shared.inflight.lock().unwrap().remove(id) {
                    handle.abort();
                }
            }
            return true;
        }
        _ => {}
    }

    // Track run-end bookkeeping for system/result frames.
    update_run_state(shared, &message);

    // Forward to the consumer, charging the frame against the byte budget.
    // This await blocks only once `DATA_BUDGET_BYTES` of frames are unconsumed
    // — that is the backpressure point. Control frames returned above and never
    // reach here, so a slow consumer cannot starve them before that point.
    let weight = approx_size(&message).clamp(1, DATA_BUDGET_BYTES) as u32;
    let permit = shared
        .data_budget
        .clone()
        .acquire_many_owned(weight)
        .await
        .ok();
    tx.send(Ok(Queued {
        value: message,
        _permit: permit,
    }))
    .is_ok()
}

async fn handle_control_request(shared: &Arc<Shared>, message: &Value) -> Result<Value> {
    let request = message
        .get("request")
        .ok_or_else(|| Error::control("control request missing 'request'"))?;
    let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");
    match subtype {
        "can_use_tool" => handle_can_use_tool(shared, request).await,
        "hook_callback" => handle_hook_callback(shared, request).await,
        "mcp_message" => handle_mcp_message(shared, request).await,
        other => Err(Error::control(format!(
            "Unsupported control request subtype: {other}"
        ))),
    }
}

async fn handle_can_use_tool(shared: &Arc<Shared>, request: &Value) -> Result<Value> {
    let callback = shared
        .can_use_tool
        .as_ref()
        .ok_or_else(|| Error::callback("canUseTool callback is not provided"))?;
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::control("can_use_tool missing tool_name"))?
        .to_string();
    let input = request.get("input").cloned().unwrap_or(json!({}));
    let suggestions = request
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| crate::types::PermissionUpdate::from_wire(v).ok())
                .collect()
        })
        .unwrap_or_default();
    let context = ToolPermissionContext {
        suggestions,
        tool_use_id: request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent_id: request
            .get("agent_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        blocked_path: request
            .get("blocked_path")
            .and_then(Value::as_str)
            .map(str::to_string),
        decision_reason: request
            .get("decision_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        title: request
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        display_name: request
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: request
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    let result = callback.call(tool_name, input.clone(), context).await?;
    Ok(match result {
        PermissionResult::Allow {
            updated_input,
            updated_permissions,
        } => {
            let mut response = json!({
                "behavior": "allow",
                "updatedInput": updated_input.unwrap_or(input),
            });
            if let Some(updates) = updated_permissions {
                let mut wire = Vec::with_capacity(updates.len());
                let mut all_ok = true;
                for update in &updates {
                    match update.to_wire() {
                        Ok(value) => wire.push(value),
                        Err(err) => {
                            tracing::warn!(
                                "can_use_tool allowed the call but a permission update could not be sent: {err}"
                            );
                            all_ok = false;
                            break;
                        }
                    }
                }
                if all_ok {
                    response["updatedPermissions"] = Value::Array(wire);
                }
            }
            response
        }
        PermissionResult::Deny { message, interrupt } => {
            let mut response = json!({"behavior": "deny", "message": message});
            if interrupt {
                response["interrupt"] = json!(true);
            }
            response
        }
    })
}

async fn handle_hook_callback(shared: &Arc<Shared>, request: &Value) -> Result<Value> {
    let callback_id = request
        .get("callback_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::control("hook_callback missing callback_id"))?;
    let callback = shared
        .hook_callbacks
        .get(callback_id)
        .ok_or_else(|| Error::callback(format!("No hook callback found for ID: {callback_id}")))?;
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let tool_use_id = request
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let output = callback
        .call(input, tool_use_id, HookContext::default())
        .await?;
    Ok(serde_json::to_value(output)?)
}

async fn handle_mcp_message(shared: &Arc<Shared>, request: &Value) -> Result<Value> {
    let server_name = request
        .get("server_name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::control("mcp_message missing server_name"))?;
    let message = request
        .get("message")
        .ok_or_else(|| Error::control("mcp_message missing message"))?;
    let server = shared
        .sdk_mcp_servers
        .get(server_name)
        .ok_or_else(|| Error::control(format!("Server '{server_name}' not found")))?;
    let response = server.handle_jsonrpc(message).await;
    // A notification gets no reply, but the control request still wants an ack.
    let mcp_response = response.unwrap_or_else(|| json!({"jsonrpc": "2.0", "result": {}}));
    Ok(json!({ "mcp_response": mcp_response }))
}

/// Update the run-end bookkeeping and signal when the run has ended.
///
/// Only `result` and `system` frames can change run-end state, so every other
/// frame type — the high-frequency ones (`assistant`, `stream_event`, `user`)
/// — returns immediately without taking the lock.
fn update_run_state(shared: &Arc<Shared>, message: &Value) {
    let msg_type = message.get("type").and_then(Value::as_str).unwrap_or("");
    if msg_type != "result" && msg_type != "system" {
        return;
    }
    let subtype = message.get("subtype").and_then(Value::as_str);

    let mut run = shared.run.lock().unwrap();

    if msg_type == "system" {
        track_task_lifecycle(&mut run, message);
        if subtype == Some("session_state_changed") {
            let state = message
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string);
            run.session_state = state.clone();
            if state.as_deref() == Some("idle")
                && run.result_received
                && run.inflight_tasks.is_empty()
            {
                end_run(shared, &mut run);
            }
        }
    }

    if msg_type == "result" {
        run.result_received = true;
        if message.get("is_error").and_then(Value::as_bool) == Some(true) {
            run.last_error_result = Some(message.clone());
        } else {
            run.last_error_result = None;
        }
        let session_idle = matches!(run.session_state.as_deref(), None | Some("idle"));
        if (session_idle || !shared.has_bidirectional_needs) && run.inflight_tasks.is_empty() {
            end_run(shared, &mut run);
        }
    } else if !(msg_type == "system" && subtype == Some("session_state_changed")) {
        run.last_error_result = None;
    }
}

fn track_task_lifecycle(run: &mut RunState, message: &Value) {
    let subtype = message.get("subtype").and_then(Value::as_str);
    let Some(task_id) = message.get("task_id").and_then(Value::as_str) else {
        return;
    };
    match subtype {
        Some("task_started") => {
            let task_type = message
                .get("task_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if DEFERRING_TASK_TYPES.contains(&task_type) {
                run.inflight_tasks.insert(task_id.to_string());
            }
        }
        Some("task_notification") => {
            run.inflight_tasks.remove(task_id);
        }
        Some("task_updated") => {
            let status = message
                .get("patch")
                .and_then(|p| p.get("status"))
                .and_then(Value::as_str);
            if let Some(status) = status {
                if TERMINAL_TASK_STATUSES.contains(&status) {
                    run.inflight_tasks.remove(task_id);
                }
            }
        }
        _ => {}
    }
}

fn end_run(shared: &Arc<Shared>, run: &mut RunState) {
    if !run.ended {
        run.ended = true;
        shared.run_ended.notify_waiters();
    }
}

async fn enrich_error(shared: &Arc<Shared>, err: Error) -> Error {
    if let Error::Process { exit_code, .. } = &err {
        let last = shared.run.lock().unwrap().last_error_result.clone();
        if let Some(data) = last {
            let text = error_result_text(&data);
            return Error::Result(Box::new(ResultError::new(
                format!("Claude Code returned an error result: {text}"),
                data,
                *exit_code,
            )));
        }
    }
    err
}

/// Pick the most informative text from an error `result` frame.
fn error_result_text(message: &Value) -> String {
    let errors = crate::errors::normalize_result_errors(message.get("errors"));
    if !errors.is_empty() {
        return errors.join("; ");
    }
    if let Some(result) = message.get("result").and_then(Value::as_str) {
        if !result.trim().is_empty() {
            return result.trim().to_string();
        }
    }
    if let Some(subtype) = message.get("subtype").and_then(Value::as_str) {
        if !subtype.is_empty() && subtype != "success" {
            return subtype.to_string();
        }
    }
    if let Some(status) = message.get("api_error_status").and_then(Value::as_i64) {
        return format!("API error (HTTP {status})");
    }
    "unknown error".to_string()
}

fn build_hook_registry(
    options: &ClaudeAgentOptions,
) -> (HashMap<String, HookCallback>, Vec<HookRegistration>) {
    let mut callbacks = HashMap::new();
    let mut registrations = Vec::new();
    let mut next_id = 0usize;
    let Some(hooks) = &options.hooks else {
        return (callbacks, registrations);
    };
    // Stable order: sort events by their wire name.
    let mut events: Vec<_> = hooks.keys().cloned().collect();
    events.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for event in events {
        let matchers = &hooks[&event];
        for matcher in matchers {
            let mut callback_ids = Vec::with_capacity(matcher.hooks.len());
            for callback in &matcher.hooks {
                let id = format!("hook_{next_id}");
                next_id += 1;
                callbacks.insert(id.clone(), callback.clone());
                callback_ids.push(id);
            }
            registrations.push(HookRegistration {
                event: event.as_str().to_string(),
                matcher: matcher.matcher.clone(),
                timeout: matcher.timeout,
                on_failure: matcher.on_failure.map(|f| f.as_str().to_string()),
                callback_ids,
            });
        }
    }
    (callbacks, registrations)
}

fn system_prompt_flags(
    system_prompt: &Option<crate::types::SystemPrompt>,
) -> (Option<bool>, Option<bool>) {
    use crate::types::SystemPrompt;
    match system_prompt {
        Some(SystemPrompt::Preset {
            exclude_dynamic_sections,
            snapshot,
            ..
        }) => (*exclude_dynamic_sections, *snapshot),
        Some(SystemPrompt::Custom { snapshot, .. }) => (None, *snapshot),
        _ => (None, None),
    }
}

fn default_control_timeout() -> Duration {
    Duration::from_secs(60)
}

/// A small non-cryptographic random for request-id uniqueness.
fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Mix with a per-call counter to avoid collisions within the same nanosecond.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
    nanos
        .wrapping_mul(2654435761)
        .wrapping_add(c.wrapping_mul(40503))
}
