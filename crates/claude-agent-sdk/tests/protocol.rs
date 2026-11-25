//! End-to-end protocol tests driving the real control engine through a mock
//! [`Transport`]. No subprocess, no network: a scripted transport plays the
//! part of the CLI, so these exercise the initialize handshake, message
//! forwarding, and the `can_use_tool` / hook / SDK-MCP control round-trips.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use claude_agent_sdk::{
    create_sdk_mcp_server, query_with_transport, tool, CanUseTool, ClaudeAgentOptions,
    ContentBlock, HookCallback, HookEvent, HookJsonOutput, HookMatcher, HookOnFailure, Message,
    PermissionResult, RawMessageStream, Result as SdkResult, ToolResponse, Transport,
};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Reaction callback: inspect a frame the engine wrote and optionally push
/// frames back onto the read stream.
type React = Arc<dyn Fn(&Value, &mpsc::UnboundedSender<SdkResult<Value>>) + Send + Sync>;

/// A scripted transport. `react` is called for every frame the engine writes
/// and may push frames back onto the read stream (simulating the CLI).
struct MockTransport {
    tx: mpsc::UnboundedSender<SdkResult<Value>>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<SdkResult<Value>>>>,
    writes: Arc<Mutex<Vec<Value>>>,
    react: React,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl MockTransport {
    fn new(
        react: impl Fn(&Value, &mpsc::UnboundedSender<SdkResult<Value>>) + Send + Sync + 'static,
    ) -> (Self, Arc<Mutex<Vec<Value>>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let transport = MockTransport {
            tx,
            rx: Mutex::new(Some(rx)),
            writes: writes.clone(),
            react: Arc::new(react),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (transport, writes)
    }
}

#[async_trait::async_trait]
impl Transport for MockTransport {
    async fn connect(&mut self) -> SdkResult<()> {
        Ok(())
    }

    fn take_message_stream(&mut self) -> Option<RawMessageStream> {
        let rx = self.rx.lock().unwrap().take()?;
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Some(Box::pin(stream))
    }

    async fn write(&self, data: String) -> SdkResult<()> {
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            let value: Value = serde_json::from_str(line).expect("valid JSON frame");
            self.writes.lock().unwrap().push(value.clone());
            (self.react)(&value, &self.tx);
        }
        Ok(())
    }

    async fn end_input(&self) -> SdkResult<()> {
        Ok(())
    }

    async fn close(&self) -> SdkResult<()> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn is_ready(&self) -> bool {
        !self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Reply to an `initialize` control request with a success response.
fn answer_initialize(frame: &Value, tx: &mpsc::UnboundedSender<SdkResult<Value>>) -> bool {
    if frame.get("type").and_then(Value::as_str) == Some("control_request")
        && frame
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("initialize")
    {
        let request_id = frame.get("request_id").cloned().unwrap_or(Value::Null);
        let _ = tx.send(Ok(json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": {"commands": [], "agents": []}},
        })));
        return true;
    }
    false
}

fn assistant_frame(text: &str) -> Value {
    json!({
        "type": "assistant",
        "message": {"model": "claude-test", "content": [{"type": "text", "text": text}]},
        "session_id": "s1",
        "uuid": "u-assistant",
    })
}

fn result_frame() -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "s1",
        "result": "done",
    })
}

#[tokio::test]
async fn happy_path_streams_assistant_then_result() {
    let (transport, writes) = MockTransport::new(|frame, tx| {
        if answer_initialize(frame, tx) {
            return;
        }
        // On the user message, emit an assistant turn and a result.
        if frame.get("type").and_then(Value::as_str) == Some("user") {
            let _ = tx.send(Ok(assistant_frame("Hello from the mock")));
            let _ = tx.send(Ok(result_frame()));
        }
    });

    let mut stream = query_with_transport("hi", ClaudeAgentOptions::default(), transport)
        .await
        .expect("query starts");

    let mut texts = Vec::new();
    let mut saw_result = false;
    while let Some(message) = stream.next().await {
        match message.expect("message ok") {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::Text(t) = block {
                        texts.push(t.text.clone());
                    }
                }
            }
            Message::Result(result) => {
                assert_eq!(result.result.as_deref(), Some("done"));
                saw_result = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(texts, vec!["Hello from the mock".to_string()]);
    assert!(saw_result);

    // The engine must have sent an initialize control request and the user turn.
    let writes = writes.lock().unwrap();
    assert!(writes.iter().any(|w| {
        w.get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("initialize")
    }));
    assert!(writes
        .iter()
        .any(|w| w.get("type").and_then(Value::as_str) == Some("user")));
}

#[tokio::test]
async fn can_use_tool_allow_round_trip() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_cb = seen.clone();
    let callback = CanUseTool::new(move |tool_name, _input, _ctx| {
        let seen = seen_cb.clone();
        async move {
            seen.lock().unwrap().push(tool_name.clone());
            // Allow Bash with a tweaked input; deny anything else.
            if tool_name == "Bash" {
                Ok(PermissionResult::allow_with_input(
                    json!({"command": "ls -la"}),
                ))
            } else {
                Ok(PermissionResult::deny("nope"))
            }
        }
    });

    let (transport, writes) = MockTransport::new(|frame, tx| {
        if answer_initialize(frame, tx) {
            return;
        }
        if frame.get("type").and_then(Value::as_str) == Some("user") {
            // Ask the SDK whether Bash may run.
            let _ = tx.send(Ok(json!({
                "type": "control_request",
                "request_id": "cli-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "ls"},
                    "tool_use_id": "toolu_1",
                    "permission_suggestions": null,
                    "blocked_path": null,
                },
            })));
        }
        // When the SDK answers the permission request, finish the run.
        if frame.get("type").and_then(Value::as_str) == Some("control_response") {
            let _ = tx.send(Ok(result_frame()));
        }
    });

    let options = ClaudeAgentOptions::builder().can_use_tool(callback).build();
    let mut stream = query_with_transport("run ls", options, transport)
        .await
        .expect("query starts");
    while let Some(message) = stream.next().await {
        if matches!(message.expect("message ok"), Message::Result(_)) {
            break;
        }
    }

    assert_eq!(seen.lock().unwrap().clone(), vec!["Bash".to_string()]);
    // The SDK's permission answer must be an allow carrying the updated input.
    let writes = writes.lock().unwrap();
    let answer = writes
        .iter()
        .find_map(|w| {
            let response = w.get("response")?;
            let behavior = response.get("response")?.get("behavior")?.as_str()?;
            if w.get("type")?.as_str()? == "control_response" {
                Some((behavior.to_string(), response.get("response").cloned()))
            } else {
                None
            }
        })
        .expect("a control_response answer");
    assert_eq!(answer.0, "allow");
    let payload = answer.1.unwrap();
    assert_eq!(payload["updatedInput"]["command"], "ls -la");
}

#[tokio::test]
async fn hook_callback_round_trip() {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired_cb = fired.clone();
    let hook = HookCallback::new(move |_input, _tool_use_id, _ctx| {
        let fired = fired_cb.clone();
        async move {
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(HookJsonOutput::default())
        }
    });
    let mut hooks = std::collections::HashMap::new();
    hooks.insert(HookEvent::PreToolUse, vec![HookMatcher::all(vec![hook])]);

    let captured_callback_id = Arc::new(Mutex::new(None::<String>));
    let captured = captured_callback_id.clone();
    let (transport, _writes) = MockTransport::new(move |frame, tx| {
        // Learn the callback id the SDK registered in its initialize request.
        if frame
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("initialize")
        {
            if let Some(id) =
                frame["request"]["hooks"]["PreToolUse"][0]["hookCallbackIds"][0].as_str()
            {
                *captured.lock().unwrap() = Some(id.to_string());
            }
            let request_id = frame.get("request_id").cloned().unwrap_or(Value::Null);
            let _ = tx.send(Ok(json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": {}},
            })));
            return;
        }
        if frame.get("type").and_then(Value::as_str) == Some("user") {
            let callback_id = captured.lock().unwrap().clone().expect("callback id");
            let _ = tx.send(Ok(json!({
                "type": "control_request",
                "request_id": "cli-hook-1",
                "request": {
                    "subtype": "hook_callback",
                    "callback_id": callback_id,
                    "input": {"hook_event_name": "PreToolUse", "tool_name": "Bash"},
                    "tool_use_id": "toolu_1",
                },
            })));
        }
        if frame.get("type").and_then(Value::as_str) == Some("control_response") {
            let _ = tx.send(Ok(result_frame()));
        }
    });

    let options = ClaudeAgentOptions::builder().hooks(hooks).build();
    let mut stream = query_with_transport("go", options, transport)
        .await
        .expect("query starts");
    while let Some(message) = stream.next().await {
        if matches!(message.expect("ok"), Message::Result(_)) {
            break;
        }
    }
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst), "hook ran");
}

#[tokio::test]
async fn sdk_mcp_tool_round_trip() {
    let add = tool(
        "add",
        "Add two numbers",
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}),
        |args| async move {
            let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
            Ok(ToolResponse::text(format!("Sum: {sum}")))
        },
    );
    let server = create_sdk_mcp_server("calc", "1.0.0", vec![add]);
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "calc".to_string(),
        claude_agent_sdk::McpServerConfig::Sdk(server),
    );

    let (transport, writes) = MockTransport::new(|frame, tx| {
        if answer_initialize(frame, tx) {
            return;
        }
        if frame.get("type").and_then(Value::as_str) == Some("user") {
            // Call the in-process tool over the control channel.
            let _ = tx.send(Ok(json!({
                "type": "control_request",
                "request_id": "cli-mcp-1",
                "request": {
                    "subtype": "mcp_message",
                    "server_name": "calc",
                    "message": {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                                "params": {"name": "add", "arguments": {"a": 2, "b": 40}}},
                },
            })));
        }
        if frame.get("type").and_then(Value::as_str) == Some("control_response")
            && frame["response"]["request_id"] == json!("cli-mcp-1")
        {
            let _ = tx.send(Ok(result_frame()));
        }
    });

    let options = ClaudeAgentOptions::builder()
        .mcp_servers(claude_agent_sdk::McpServers::Map(servers))
        .build();
    let mut stream = query_with_transport("add them", options, transport)
        .await
        .expect("query starts");
    while let Some(message) = stream.next().await {
        if matches!(message.expect("ok"), Message::Result(_)) {
            break;
        }
    }

    let writes = writes.lock().unwrap();
    let mcp_answer = writes
        .iter()
        .find(|w| w["response"]["request_id"] == json!("cli-mcp-1"))
        .expect("mcp answer written");
    let inner = &mcp_answer["response"]["response"]["mcp_response"];
    assert_eq!(inner["result"]["content"][0]["text"], "Sum: 42");
}

/// Control frames must never be starved by a consumer that is not reading.
///
/// The mock floods 2000 data frames and *then* asks a permission question. The
/// test never polls the message stream, so with a design that forwards data
/// through a small bounded queue the router would block after the first few
/// frames and the permission request would never be reached (the CLI, waiting
/// on it, would hang). With the byte-budgeted split, the answer is written
/// promptly regardless of consumer speed.
#[tokio::test]
async fn control_answered_while_consumer_stalls() {
    let callback =
        CanUseTool::new(|_tool, _input, _ctx| async move { Ok(PermissionResult::allow()) });

    let (transport, writes) = MockTransport::new(|frame, tx| {
        if answer_initialize(frame, tx) {
            return;
        }
        if frame.get("type").and_then(Value::as_str) == Some("user") {
            for i in 0..2000 {
                let _ = tx.send(Ok(assistant_frame(&format!("frame {i}"))));
            }
            let _ = tx.send(Ok(json!({
                "type": "control_request",
                "request_id": "cli-stall-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Read",
                    "input": {"file_path": "/tmp/x"},
                    "tool_use_id": "toolu_stall",
                    "permission_suggestions": null,
                    "blocked_path": null,
                },
            })));
        }
    });

    let options = ClaudeAgentOptions::builder().can_use_tool(callback).build();
    let stream = query_with_transport("go", options, transport)
        .await
        .expect("query starts");

    // Deliberately never poll `stream`; wait for the control answer anyway.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let answered = writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| w["response"]["request_id"] == json!("cli-stall-1"));
        if answered {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "control request was starved behind unconsumed data frames"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let behavior = writes
        .lock()
        .unwrap()
        .iter()
        .find(|w| w["response"]["request_id"] == json!("cli-stall-1"))
        .map(|w| w["response"]["response"]["behavior"].clone())
        .unwrap();
    assert_eq!(behavior, json!("allow"));
    drop(stream);
}

fn block_hook_options() -> ClaudeAgentOptions {
    let hook = HookCallback::new(|_i, _t, _c| async move { Ok(HookJsonOutput::default()) });
    ClaudeAgentOptions::builder()
        .hook(
            HookEvent::PreToolUse,
            HookMatcher::all(vec![hook]).with_on_failure(HookOnFailure::Block),
        )
        .build()
}

/// `on_failure: Block` must fail *closed*: a CLI that doesn't advertise
/// `hook_on_failure_supported` would silently run the hook fail-open, so the
/// connect is refused (mirrors the Python SDK's guard).
#[tokio::test]
async fn on_failure_block_refused_by_old_cli() {
    let (transport, _writes) = MockTransport::new(|frame, tx| {
        // An initialize answer *without* hook_on_failure_supported.
        answer_initialize(frame, tx);
    });
    let err = query_with_transport("go", block_hook_options(), transport)
        .await
        .err()
        .expect("connect must be refused");
    assert!(
        matches!(err, claude_agent_sdk::Error::CliConnection(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("on_failure"));
}

#[tokio::test]
async fn on_failure_block_accepted_when_supported() {
    let (transport, _writes) = MockTransport::new(|frame, tx| {
        if frame["request"]["subtype"] == json!("initialize") {
            let request_id = frame.get("request_id").cloned().unwrap_or(Value::Null);
            let _ = tx.send(Ok(json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id,
                             "response": {"hook_on_failure_supported": true, "hooks_applied": true}},
            })));
        } else if frame["type"] == json!("user") {
            let _ = tx.send(Ok(result_frame()));
        }
    });
    let mut stream = query_with_transport("go", block_hook_options(), transport)
        .await
        .expect("connect accepted");
    while let Some(message) = stream.next().await {
        if matches!(message.expect("ok"), Message::Result(_)) {
            break;
        }
    }
}

/// A `control_cancel_request` must abort the in-flight handler so no stale
/// response is written for a request the CLI has abandoned.
#[tokio::test]
async fn cancel_aborts_inflight_control_handler() {
    let hook = HookCallback::new(|_i, _t, _c| async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(HookJsonOutput::default())
    });
    let options = ClaudeAgentOptions::builder()
        .hook(HookEvent::PreToolUse, HookMatcher::all(vec![hook]))
        .build();

    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_cb = captured.clone();
    let (transport, writes) = MockTransport::new(move |frame, tx| {
        if frame["request"]["subtype"] == json!("initialize") {
            if let Some(id) =
                frame["request"]["hooks"]["PreToolUse"][0]["hookCallbackIds"][0].as_str()
            {
                *captured_cb.lock().unwrap() = Some(id.to_string());
            }
            let request_id = frame.get("request_id").cloned().unwrap_or(Value::Null);
            let _ = tx.send(Ok(json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": {}},
            })));
            return;
        }
        if frame["type"] == json!("user") {
            let callback_id = captured_cb.lock().unwrap().clone().expect("callback id");
            let _ = tx.send(Ok(json!({
                "type": "control_request",
                "request_id": "cli-cancel-1",
                "request": {"subtype": "hook_callback", "callback_id": callback_id,
                            "input": {"hook_event_name": "PreToolUse"}, "tool_use_id": null},
            })));
            // The CLI changes its mind right away.
            let _ = tx.send(Ok(
                json!({"type": "control_cancel_request", "request_id": "cli-cancel-1"}),
            ));
        }
    });

    let stream = query_with_transport("go", options, transport)
        .await
        .expect("query starts");
    // Longer than the hook's own sleep: a non-aborted handler would have
    // answered by now.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let answered = writes
        .lock()
        .unwrap()
        .iter()
        .any(|w| w["response"]["request_id"] == json!("cli-cancel-1"));
    assert!(!answered, "cancelled control request must not be answered");
    drop(stream);
}

/// Dropping a stream after its runtime is gone must not panic (a bare
/// `tokio::spawn` in `Drop` would).
#[test]
fn drop_after_runtime_shutdown_does_not_panic() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let stream = runtime.block_on(async {
        let (transport, _writes) = MockTransport::new(|frame, tx| {
            answer_initialize(frame, tx);
        });
        query_with_transport("go", ClaudeAgentOptions::default(), transport)
            .await
            .expect("query starts")
    });
    drop(runtime);
    drop(stream);
}
