//! End-to-end HTTP tests: a real `axum` server on an ephemeral port, exercised
//! with `reqwest`. Agent routes are driven against the SDK's fake `claude`
//! fixture (no network, no API); the direct Messages API client is driven
//! against a tiny mock upstream that checks the headers the real API needs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_runtime::{AppState, Config, app, build};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../claude-agent-sdk/tests/fixtures/fake_cli.sh")
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Serve `router` on an ephemeral loopback port; returns its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn test_config() -> Config {
    Config {
        cli_path: Some(fake_cli()),
        ..Config::default()
    }
}

#[tokio::test]
async fn health_is_open() {
    let base = serve(app(Config {
        auth_token: Some("secret".into()),
        ..test_config()
    }))
    .await;
    let body: Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn protected_routes_require_the_token() {
    let base = serve(app(Config {
        auth_token: Some("secret".into()),
        ..test_config()
    }))
    .await;
    let client = reqwest::Client::new();
    let denied = client
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let wrong = client
        .post(format!("{base}/agent/run"))
        .bearer_auth("nope")
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let right = client
        .get(format!("{base}/agent/sessions"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(right.status(), 200);
}

#[tokio::test]
async fn agent_run_returns_the_final_result() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hello runtime", "max_turns": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"], "echo: hello runtime");
    assert_eq!(body["session_id"], "fake-session");
    assert_eq!(body["is_error"], false);
    assert!(body["message_count"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn agent_run_rejects_cwd_unless_allowed() {
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hi", "cwd": "/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn agent_run_rejects_empty_prompt() {
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn agent_stream_emits_sse_events() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/stream"))
        .json(&json!({"prompt": "stream me"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("event: assistant"), "{body}");
    assert!(body.contains("event: result"), "{body}");
    assert!(body.contains("echo: stream me"), "{body}");
    assert!(
        body.trim_end().ends_with("event: done\ndata: ") || body.contains("event: done"),
        "{body}"
    );
}

/// A stand-in for `api.anthropic.com` that checks the headers the real API
/// requires and returns a canned Messages response. The first `failures`
/// calls answer `529 overloaded` (with a `retry-after`) instead.
fn mock_upstream(hits: Arc<AtomicUsize>, failures: usize) -> Router {
    #[derive(Clone)]
    struct Upstream {
        hits: Arc<AtomicUsize>,
        failures: usize,
    }
    async fn messages(
        State(upstream): State<Upstream>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let hit = upstream.hits.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(
            headers.get("authorization").is_none(),
            "must not send Authorization"
        );
        assert_eq!(body["messages"][0]["content"], "Hello, Claude!");
        if hit <= upstream.failures {
            return (
                axum::http::StatusCode::from_u16(529).unwrap(),
                [("retry-after", "0")],
                Json(json!({"type": "error", "error": {"type": "overloaded_error"}})),
            )
                .into_response();
        }
        Json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [{"type": "text", "text": "Hello from the mock API"}],
        }))
        .into_response()
    }
    Router::new()
        .route("/v1/messages", post(messages))
        .with_state(Upstream { hits, failures })
}

#[tokio::test]
async fn messages_complete_calls_the_api_correctly() {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = serve(mock_upstream(hits.clone(), 0)).await;
    let base = serve(app(Config {
        anthropic_api_key: Some("test-key".into()),
        anthropic_base_url: upstream,
        ..Config::default()
    }))
    .await;

    let body: Value = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "Hello, Claude!", "model": "claude-sonnet-5-5"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["text"], "Hello from the mock API");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn messages_routes_need_a_server_key() {
    let base = serve(app(Config::default())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
}

/// A `529 overloaded` is retried (honouring `retry-after`), so a transient
/// upstream blip never reaches the client.
#[tokio::test]
async fn messages_complete_retries_transient_upstream_failures() {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = serve(mock_upstream(hits.clone(), 2)).await;
    let base = serve(app(Config {
        anthropic_api_key: Some("test-key".into()),
        anthropic_base_url: upstream,
        ..Config::default()
    }))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "Hello, Claude!"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["text"], "Hello from the mock API");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "two failures, then success");
}

/// Past the retry budget, the failure is reported (as a gateway error).
#[tokio::test]
async fn messages_complete_gives_up_after_the_retry_budget() {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = serve(mock_upstream(hits.clone(), 10)).await;
    let base = serve(app(Config {
        anthropic_api_key: Some("test-key".into()),
        anthropic_base_url: upstream,
        ..Config::default()
    }))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "Hello, Claude!"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("529"), "{body}");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "one attempt plus two retries"
    );
}

// ---------------------------------------------------------------------------
// Capacity: kept sessions, concurrency limit, run timeout
// ---------------------------------------------------------------------------

async fn serve_built(config: Config) -> (String, Arc<AppState>) {
    let (router, state) = build(config);
    (serve(router).await, state)
}

async fn run(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn stream(base: &str, body: Value) -> String {
    reqwest::Client::new()
        .post(format!("{base}/agent/stream"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn sessions(base: &str) -> Value {
    reqwest::get(format!("{base}/agent/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll until `running` turns are in flight (spawning the fake CLI takes a
/// moment, and a slot is released only once its process is gone).
async fn wait_for_running(base: &str, running: u64) {
    for _ in 0..500 {
        if sessions(base).await["running"] == running {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "never saw {running} running turn(s): {}",
        sessions(base).await
    );
}

#[tokio::test]
async fn one_shot_runs_leave_no_session_behind() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(test_config()).await;
    let body: Value = run(&base, json!({"prompt": "once"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["session_kept"], false);
    assert_eq!(body["session_reused"], false);
    assert!(state.pool.is_empty());
    wait_for_running(&base, 0).await;
}

#[tokio::test]
async fn kept_sessions_are_reused_across_requests() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(test_config()).await;

    let first = run(&base, json!({"prompt": "first", "keep_session": true})).await;
    assert_eq!(first.status(), 200, "{}", first.text().await.unwrap());
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["session_kept"], true);
    assert_eq!(first["session_reused"], false);
    assert_eq!(first["num_turns"], 1);
    let id = first["session_id"].as_str().unwrap().to_string();
    assert_eq!(state.pool.len(), 1);

    // The fake CLI counts turns per process, so a second turn in the same
    // process reports 2: proof the process (and its conversation) was reused.
    let second = run(&base, json!({"prompt": "second", "session_id": id})).await;
    assert_eq!(second.status(), 200, "{}", second.text().await.unwrap());
    let second: Value = second.json().await.unwrap();
    assert_eq!(second["session_reused"], true);
    assert_eq!(second["session_kept"], true);
    assert_eq!(second["num_turns"], 2);
    assert_eq!(second["result"], "echo: second");

    let list = sessions(&base).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["sessions"][0]["id"], id);
    assert_eq!(list["running"], 0);

    // A last turn that lets the session go.
    let third: Value = run(
        &base,
        json!({"prompt": "third", "session_id": id, "keep_session": false}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(third["num_turns"], 3);
    assert_eq!(third["session_kept"], false);
    assert!(state.pool.is_empty());

    let gone = run(&base, json!({"prompt": "again", "session_id": id})).await;
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn kept_sessions_can_be_closed_explicitly() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(test_config()).await;
    let first: Value = run(&base, json!({"prompt": "keep me", "keep_session": true}))
        .await
        .json()
        .await
        .unwrap();
    let id = first["session_id"].as_str().unwrap().to_string();
    assert_eq!(state.pool.len(), 1);

    let client = reqwest::Client::new();
    let closed = client
        .delete(format!("{base}/agent/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 204);
    assert!(state.pool.is_empty());
    let again = client
        .delete(format!("{base}/agent/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
}

#[tokio::test]
async fn idle_sessions_are_reaped() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(Config {
        session_ttl: Duration::from_millis(200),
        ..test_config()
    })
    .await;
    run(&base, json!({"prompt": "keep me", "keep_session": true})).await;
    assert_eq!(state.pool.len(), 1);
    for _ in 0..100 {
        if state.pool.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("idle session was not reaped");
}

#[tokio::test]
async fn a_hung_turn_times_out_with_504_and_frees_its_slot() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(Config {
        run_timeout: Duration::from_millis(300),
        ..test_config()
    })
    .await;
    let response = run(
        &base,
        json!({"prompt": "HANG for a while", "keep_session": true}),
    )
    .await;
    assert_eq!(response.status(), 504);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("timeout"),
        "{body}"
    );
    // Nothing is kept, and the slot comes back once the process is gone.
    assert!(state.pool.is_empty());
    wait_for_running(&base, 0).await;
}

#[tokio::test]
async fn runs_beyond_the_concurrency_limit_get_429() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, _state) = serve_built(Config {
        max_concurrent_runs: 1,
        run_timeout: Duration::from_secs(3),
        ..test_config()
    })
    .await;
    let hung = {
        let base = base.clone();
        tokio::spawn(async move { run(&base, json!({"prompt": "HANG"})).await })
    };
    wait_for_running(&base, 1).await;

    let rejected = run(&base, json!({"prompt": "quick"})).await;
    assert_eq!(rejected.status(), 429);
    let body: Value = rejected.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("concurrent"),
        "{body}"
    );
    // Cheap rejections come before the slot check, so a bad request is never
    // reported as "too busy".
    assert_eq!(run(&base, json!({"prompt": "   "})).await.status(), 400);
    assert_eq!(
        run(&base, json!({"prompt": "x", "session_id": "nope"}))
            .await
            .status(),
        404
    );

    assert_eq!(hung.await.unwrap().status(), 504);
    wait_for_running(&base, 0).await;
    // Capacity is back.
    assert_eq!(run(&base, json!({"prompt": "quick"})).await.status(), 200);
}

#[tokio::test]
async fn a_busy_session_rejects_a_second_turn_with_409() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(Config {
        run_timeout: Duration::from_secs(3),
        ..test_config()
    })
    .await;
    let first: Value = run(&base, json!({"prompt": "keep", "keep_session": true}))
        .await
        .json()
        .await
        .unwrap();
    let id = first["session_id"].as_str().unwrap().to_string();
    let hung = {
        let base = base.clone();
        let id = id.clone();
        tokio::spawn(async move { run(&base, json!({"prompt": "HANG", "session_id": id})).await })
    };
    wait_for_running(&base, 1).await;

    let busy = run(&base, json!({"prompt": "me too", "session_id": id})).await;
    assert_eq!(busy.status(), 409);
    let closing = reqwest::Client::new()
        .delete(format!("{base}/agent/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(closing.status(), 409);

    assert_eq!(hung.await.unwrap().status(), 504);
    // A timed-out session is not trusted again.
    assert!(state.pool.is_empty());
}

#[tokio::test]
async fn stream_can_keep_a_session_for_a_follow_up_run() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(test_config()).await;
    let body = stream(&base, json!({"prompt": "one", "keep_session": true})).await;
    assert!(body.contains("event: result"), "{body}");
    assert!(body.contains("event: done"), "{body}");
    assert!(body.contains(r#""session_kept":true"#), "{body}");
    assert_eq!(state.pool.len(), 1);

    let second: Value = run(
        &base,
        json!({"prompt": "two", "session_id": "fake-session"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(second["num_turns"], 2);
    assert_eq!(second["session_reused"], true);
}

#[tokio::test]
async fn stream_reports_a_timeout_as_an_error_event() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(Config {
        run_timeout: Duration::from_millis(300),
        ..test_config()
    })
    .await;
    let body = stream(&base, json!({"prompt": "HANG", "keep_session": true})).await;
    // The fake echoes before hanging, so progress was streamed...
    assert!(body.contains("event: assistant"), "{body}");
    // ...then the timeout is reported and the stream still ends cleanly.
    assert!(body.contains("event: error"), "{body}");
    assert!(body.contains("timeout"), "{body}");
    assert!(body.contains("event: done"), "{body}");
    assert!(body.contains(r#""session_kept":false"#), "{body}");
    assert!(state.pool.is_empty());
}

/// A streaming consumer that goes away mid-turn frees the slot promptly: the
/// turn is abandoned and its process terminated rather than running on until
/// the timeout.
#[tokio::test]
async fn a_disconnected_stream_consumer_frees_the_slot() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (base, state) = serve_built(Config {
        run_timeout: Duration::from_secs(30),
        ..test_config()
    })
    .await;
    let mut response = reqwest::Client::new()
        .post(format!("{base}/agent/stream"))
        .json(&json!({"prompt": "HANG", "keep_session": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    wait_for_running(&base, 1).await;
    // Read the first event, then hang up.
    let first = response.chunk().await.unwrap().unwrap();
    assert!(
        String::from_utf8_lossy(&first).contains("event:"),
        "{first:?}"
    );
    drop(response);
    // Well within the 5 s the poll allows, far below the 30 s run timeout.
    wait_for_running(&base, 0).await;
    assert!(state.pool.is_empty());
}
